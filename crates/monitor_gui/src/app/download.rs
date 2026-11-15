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
use tokio::task::AbortHandle;

use crate::dialogs::BackgroundEvent;
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
    /// Set on the way out. Checked between chunks as well as aborted at the
    /// task's next await point, since a write in progress is not at one.
    cancel: Arc<AtomicBool>,
    abort: AbortHandle,
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

    pub(super) fn start(&mut self, id: u64, camera: String, cancel: Arc<AtomicBool>, abort: AbortHandle) {
        self.active = Some(Active { id, camera, progress: Progress::default(), cancel, abort });
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
    /// Called from `quit()` and from `eframe::App::on_exit`, the two exits that
    /// reliably run code. The `.part` is not deleted here: on Windows an open
    /// file cannot be removed, and a process killed outright never gets here at
    /// all. The next start's sweep is what owns that cleanup.
    pub(super) fn cancel(&mut self) {
        if let Some(active) = self.active.take() {
            active.cancel.store(true, Ordering::Relaxed);
            active.abort.abort();
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
                    Ok(path) => BackgroundEvent::DownloadDone { id, path },
                    Err(reason) => {
                        let _ = std::fs::remove_file(&clip.temp);
                        BackgroundEvent::DownloadFailed { id, reason }
                    }
                },
                Err(err) => BackgroundEvent::DownloadFailed { id, reason: err.to_string() },
            };
            let _ = events.send(answer);
        });
        let abort = task.abort_handle();
        // Dropping the handle does not stop the task; the abort handle is what
        // `cancel()` keeps for the way out.
        drop(task);
        self.download.start(id, camera, cancel, abort);
    }
}

/// The panel's navigation scopes, declared beside the panel they walk.
///
/// The lengths and the button under them are two scopes rather than one, because
/// the two orientations arrange the lengths differently: a phone stacks them and
/// walks them with Up / Down, a wide screen puts them side by side and walks them
/// with Left / Right. What both have in common is where Down goes - to the one
/// button - and Up out of it - back to the lengths. The four names are the whole
/// of the difference, and a scope that names an exit that does not exist is a
/// dead key rather than a compile error, so the test at the foot of this file
/// walks them.
const LENGTHS_STACKED: &str = "download-lengths-stacked";
const LENGTHS_BESIDE: &str = "download-lengths-beside";
const SAVE_UNDER_STACK: &str = "download-save-under-stack";
const SAVE_UNDER_ROW: &str = "download-save-under-row";

pub(super) fn scopes() -> [(&'static str, ScopeDef); 4] {
    [
        (
            LENGTHS_STACKED,
            ScopeDef::new(Axis::Column).scrolling().exit(Dir::Down, Exit::Scope(SAVE_UNDER_STACK)),
        ),
        (
            LENGTHS_BESIDE,
            ScopeDef::new(Axis::Row).scrolling().exit(Dir::Down, Exit::Scope(SAVE_UNDER_ROW)),
        ),
        (SAVE_UNDER_STACK, ScopeDef::new(Axis::Row).exit(Dir::Up, Exit::Scope(LENGTHS_STACKED))),
        (SAVE_UNDER_ROW, ScopeDef::new(Axis::Row).exit(Dir::Up, Exit::Scope(LENGTHS_BESIDE))),
    ]
}

/// Draws the three lengths, in the order the walk uses.
///
/// The ring starts on the length that is chosen, not on the first one: opening a
/// panel with 5 minutes picked and the ring on 1 minute reads as though the
/// panel disagreed with itself. Where there is a button to press, that button
/// takes the ring instead - see the foot of the body - because the press that
/// opened this window already said what it wanted.
fn draw_lengths(app: &mut XgViewApp, ui: &mut egui::Ui, choice: &mut DownloadChoice, labels: &[String]) {
    for (index, label) in labels.iter().enumerate() {
        let button = app.nav.tracked(ui.selectable_label(choice.choice == index, label.clone()));
        if index == choice.choice {
            app.download_panel.anchor = Some(button.id);
        }
        if button.clicked() {
            choice.choice = index;
        }
    }
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
    pub(super) fn open_download_panel(&mut self) {
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
        // it, as every other window of this program does. Without this the panel
        // took the focus and gave it back nowhere, so the strip came back with a
        // remote that had to arm it with Enter before an arrow did anything.
        self.download_panel.from = self.download_button();
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
    /// A width that fits a phone, a height that fits a landscape television, and
    /// the two scopes [above](scopes): the three lengths are a row on a wide
    /// screen and a column on a narrow one, and Down from either is the button.
    /// The actual interval and the file name are shown *before* confirmation,
    /// because a window slid away from a hole is not the moment the viewer
    /// pointed at, and a short stretch is not the length they chose.
    ///
    /// The way out is the cross in the title bar and the Back key, as in every
    /// other window of this program - there is no Cancel button beside the
    /// answer. Back is answered in `handle_keys`, where a window this program
    /// opened takes it before the panel behind it does.
    pub(super) fn download_panel_window(&mut self, ctx: &egui::Context) {
        let screen = ctx.screen_rect();
        let width = 420.0_f32.min(screen.width() - 48.0).max(260.0);
        let stacked = screen.width() < screen.height();
        let max_height = screen.height() * 0.8;
        self.modal_frame(
            ctx,
            "xgview-download-panel",
            width,
            |app| &mut app.download_panel,
            move |app, ui, choice| {
                let seconds = DURATION_CHOICES[choice.choice.min(DURATION_CHOICES.len() - 1)];
                let plan = rules::clamp_window(&choice.stretches, choice.cursor, seconds);
                let ready = match plan {
                    ClipWindow::Ready { from, to, .. } => Some((from, to)),
                    _ => None,
                };
                let mut request: Option<ClipRequest> = None;
                let mut confirm = false;
                let mut close = false;
                let mut jump: Option<i64> = None;

                // The cross stays outside the scroll area, where every other
                // window puts it: a body long enough to scroll must not carry
                // the way out off the top of the screen with it.
                let bar = dialogs::title_bar(
                    ui,
                    &monitor_i18n::tr("download-title"),
                    true,
                    &monitor_i18n::tr("dialog-close"),
                );
                if bar.close.is_some_and(|button| button.clicked()) {
                    close = true;
                }

                egui::ScrollArea::vertical().max_height(max_height).show(ui, |ui| {
                    ui.label(egui::RichText::new(&choice.camera).strong());
                    ui.add_space(theme::space::M);

                    let labels: Vec<String> = DURATION_CHOICES
                        .iter()
                        .map(|s| {
                            monitor_i18n::tr_args("download-minutes", &[("count", (s / 60).into())])
                        })
                        .collect();
                    app.nav.open(if stacked { LENGTHS_STACKED } else { LENGTHS_BESIDE });
                    if stacked {
                        draw_lengths(app, ui, choice, &labels);
                    } else {
                        ui.horizontal(|ui| draw_lengths(app, ui, choice, &labels));
                    }
                    // A hole in the recording has nothing to save, and the two
                    // ways out of it are the lengths' own: the walk reaches them
                    // after the last one, and Down from there is the exit to a
                    // button that stays out of the reach while the window is a
                    // hole.
                    if let ClipWindow::Gap { before, after } = plan {
                        ui.add_space(theme::space::S);
                        ui.label(monitor_i18n::tr("download-gap"));
                        if let Some(at) = before {
                            let button =
                                app.nav.tracked(ui.button(monitor_i18n::tr("download-jump-before")));
                            if button.clicked() {
                                jump = Some(at);
                            }
                        }
                        if let Some(at) = after {
                            let button =
                                app.nav.tracked(ui.button(monitor_i18n::tr("download-jump-after")));
                            if button.clicked() {
                                jump = Some(at);
                            }
                        }
                    }
                    app.nav.close();
                    ui.add_space(theme::space::M);

                    match plan {
                        ClipWindow::Ready { from, to, shortened } => {
                            let name = rules::clip_file_name(
                                &choice.camera,
                                &choice.frigate_camera,
                                from,
                                to - from,
                            );
                            ui.label(monitor_i18n::tr_args(
                                "download-range",
                                &[("from", stamp(from).into()), ("to", stamp(to).into())],
                            ));
                            ui.label(monitor_i18n::tr_args(
                                "download-actual",
                                &[("seconds", (to - from).into())],
                            ));
                            if shortened {
                                ui.label(
                                    egui::RichText::new(monitor_i18n::tr("download-shortened"))
                                        .small()
                                        .color(theme::TEXT_DIM),
                                );
                            }
                            ui.label(monitor_i18n::tr_args("download-file", &[("name", name.into())]));
                            ui.label(monitor_i18n::tr_args(
                                "download-to",
                                &[("path", choice.dir.clone().into())],
                            ));
                        }
                        ClipWindow::Gap { .. } => {}
                        ClipWindow::Empty => {
                            ui.label(monitor_i18n::tr("download-none"));
                        }
                    }
                    ui.add_space(theme::space::M);
                    // A button that cannot act is not a control: it takes no
                    // focus, so it is registered only when it can be pressed.
                    // Its absence is what leaves Down from the last length with
                    // nowhere to go, which is the honest answer for a hole.
                    if ready.is_some() {
                        let save_label = monitor_i18n::tr("download-confirm");
                        ui.horizontal(|ui| {
                            ui.add_space(Self::center_offset(ui, &[&save_label]));
                            app.nav.open(if stacked { SAVE_UNDER_STACK } else { SAVE_UNDER_ROW });
                            let save = app.nav.tracked(ui.button(save_label));
                            // The panel opens on the save: the press that
                            // opened it was a request to save this recording,
                            // and the length still shows what it will save.
                            app.download_panel.anchor = Some(save.id);
                            app.nav.close();
                            if save.clicked() {
                                confirm = true;
                            }
                        });
                    }
                });

                if confirm {
                    tracing::info!(target: "xgview::app", ready = ready.is_some(), "the save panel was confirmed");
                    if let Some((from, to)) = ready {
                        let name = rules::clip_file_name(
                            &choice.camera,
                            &choice.frigate_camera,
                            from,
                            to - from,
                        );
                        request = Some(ClipRequest {
                            base_url: choice.base_url.clone(),
                            camera: choice.frigate_camera.clone(),
                            from,
                            to,
                            credentials: choice.credentials.clone(),
                            name,
                        });
                    }
                }
                if let Some(request) = request {
                    let camera = choice.camera.clone();
                    app.start_download(request, camera);
                    return Answer::CLOSE;
                }
                if let Some(at) = jump {
                    app.seek(at);
                    return Answer::CLOSE;
                }
                if close {
                    Answer::CLOSE
                } else {
                    Answer::STAY
                }
            },
        );

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
    fn down_from_the_lengths_reaches_the_button_in_both_orientations() {
        for (lengths, save) in
            [(LENGTHS_STACKED, SAVE_UNDER_STACK), (LENGTHS_BESIDE, SAVE_UNDER_ROW)]
        {
            let (mut nav, ids, save_id) = panel(lengths, save);
            assert_eq!(nav.step(ids[2], Dir::Down), Some(save_id), "{lengths}: Down reaches it");
            assert_eq!(nav.step(save_id, Dir::Up), Some(ids[0]), "{save}: Up comes back");
        }
    }

    /// A phone stacks the lengths, so the arrows that pick one are Up and Down.
    /// A sideways press is spent rather than wandering out of the panel.
    #[test]
    fn a_stacked_panel_is_walked_up_and_down() {
        let (mut nav, ids, _) = panel(LENGTHS_STACKED, SAVE_UNDER_STACK);
        assert_eq!(nav.step(ids[0], Dir::Down), Some(ids[1]));
        assert_eq!(nav.step(ids[1], Dir::Up), Some(ids[0]));
        assert_eq!(nav.step(ids[1], Dir::Left), None, "a column has no sideways walk");
    }

    /// A wide screen puts them side by side, and then the arrows that pick one
    /// are Left and Right - the ones a viewer tries first on a row.
    #[test]
    fn a_panel_beside_is_walked_left_and_right() {
        let (mut nav, ids, save_id) = panel(LENGTHS_BESIDE, SAVE_UNDER_ROW);
        assert_eq!(nav.step(ids[0], Dir::Right), Some(ids[1]));
        assert_eq!(nav.step(ids[1], Dir::Left), Some(ids[0]));
        assert_eq!(nav.step(ids[0], Dir::Down), Some(save_id), "and Down still leaves the row");
    }

    /// An abort handle for a task that has already finished is enough for the
    /// state machine: what these tests read is the state, not a running task.
    fn handle() -> AbortHandle {
        let runtime = tokio::runtime::Builder::new_current_thread().build().expect("a runtime");
        let task = runtime.spawn(async {});
        let handle = task.abort_handle();
        runtime.block_on(task).ok();
        handle
    }

    fn begun() -> (DownloadState, u64, Arc<AtomicBool>) {
        let mut state = DownloadState::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let id = state.allocate_id();
        state.start(id, "门口".into(), cancel.clone(), handle());
        (state, id, cancel)
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
}
