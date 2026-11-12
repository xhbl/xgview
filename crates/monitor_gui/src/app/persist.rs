//! The configuration on disk: the debounced save, the export and the import,
//! and the file dialog they go through.

use super::*;

/// How long a press waits for the storage permission it has asked for.
///
/// Long enough to read a system dialog and press Allow, short enough that one
/// taken away without an answer does not leave the viewer waiting on nothing.
#[cfg(target_os = "android")]
const STORAGE_WAIT: Duration = Duration::from_secs(30);

/// How long an import waits for "all files access" - a switch in the system
/// settings, so the wait covers walking there, finding it and coming back. It is
/// only the backstop: the app sees the viewer return, which ends the wait at
/// once, granted or not. See [`XgViewApp::poll_storage_pending`].
#[cfg(target_os = "android")]
const ALL_FILES_WAIT: Duration = Duration::from_secs(300);

impl XgViewApp {
    /// Recomputes the decoding schedule and hands the transitions to the
    /// pipeline. Cheap and idempotent, it is only run when something changed.
    pub(super) fn sync(&mut self) {
        if !self.needs_sync {
            return;
        }
        self.needs_sync = false;

        let count = self.config.enabled_count();
        if self.scheduler.channel_count() != count {
            self.scheduler.set_channel_count(count);
        }

        let cameras = self.config.active_cameras();
        let changes = self.scheduler.refresh(&cameras);
        if !changes.is_empty() {
            tracing::debug!(target: "xgview::gui", changes = changes.len(), "applying the channel schedule");
            self.manager.apply(&changes, &cameras);
        }
        // The capability question is asked after the schedule, never before:
        // magnifying reopens that channel on the main stream, and the probe has
        // to ask about the stream a call would go to. Both go down the same
        // command channel in the order they are sent, so this one finds the
        // channel already pulling it.
        if let Some(index) = self.scheduler.zoom() {
            self.manager.probe_talk(index);
        }
        // The playback channel sits one past the last one a camera can take, so
        // a wall that changed length leaves it on an index a camera now owns:
        // it goes first, and the recording with it.
        let playback_index = self.playback_index();
        if self.playback_is_stale(playback_index) {
            self.leave_playback();
        }
        self.channels.retain(|index, _| *index < count || *index == playback_index);
        // A channel beyond the grid is gone for good, and its textures with it.
        let dropped: Vec<usize> = self
            .textures
            .keys()
            .copied()
            .filter(|index| *index >= count && *index != playback_index)
            .collect();
        for index in dropped {
            self.video.release(index);
            self.textures.remove(&index);
        }

        let layout = self.scheduler.layout();
        let page = self.scheduler.page();
        let focus = self.scheduler.focus();
        let portrait = self.orientation.is_portrait();
        let view = self.config.group_mut();
        // The grid is remembered per orientation; page and focus are shared and
        // clamped to the current grid.
        if portrait {
            view.portrait_layout = layout;
        } else {
            view.layout = layout;
        }
        view.page = page;
        view.focus = focus;
    }

    pub(super) fn save_now(&mut self) {
        if self.persisted == self.config {
            self.dirty = false;
            return;
        }
        match self.config.save(&self.config_path) {
            Ok(()) => {
                self.persisted = self.config.clone();
                self.dirty = false;
                self.dirty_since = None;
            }
            Err(err) => {
                self.dirty = false;
                self.dirty_since = None;
                self.flash(
                    monitor_i18n::tr_args(
                        "toast-config-save-failed",
                        &[
                            ("path", self.config_path.display().to_string().into()),
                            ("error", err.to_string().into()),
                        ],
                    ),
                    ToastKind::Error,
                );
            }
        }
    }

    pub(super) fn autosave(&mut self) {
        if !self.dirty {
            return;
        }
        if self.persisted == self.config {
            self.dirty = false;
            return;
        }
        if let Some(since) = self.dirty_since {
            if since.elapsed() < SAVE_DEBOUNCE {
                return;
            }
        }
        self.save_now();
    }

    // ------------------------------------------------------------ navigation

    /// Draws the file dialog while it is up, and acts on what it answers.
    pub(super) fn poll_file_dialog(&mut self, ctx: &egui::Context) {
        // Before anything else, and whether or not a window is up: a press
        // waiting on the storage permission goes ahead from here, on the frame
        // the answer arrives. See [`Self::poll_storage_pending`].
        #[cfg(target_os = "android")]
        self.poll_storage_pending();
        // Cleared first, set again below only if this frame is the one that
        // takes the answer - so the panel drawn later in the same frame knows
        // the press that answered the dialog is spent. See
        // [`Self::file_dialog_answered`].
        self.file_dialog_answered = false;
        // The frame it goes up, whatever held the focus hands it over. The
        // button that opened the dialog is behind it now, and the dialog reads
        // its own keys only while no widget has the focus - so an Esc still
        // belonging to that button would be read by nobody, and the first press
        // would be spent doing nothing.
        if self.file_job.is_some() && !self.file_dialog_keys {
            let held = ctx.memory(|memory| memory.focused());
            // The control the export or import was asked from is the one to go
            // back to. Both buttons remember themselves when pressed - but only
            // for a keyboard or a remote, so this is `None` when the dialog came
            // from a pointer and the focus it took is the one it hands back:
            // nothing visibly moves either way.
            self.file_dialog_anchor = self.file_dialog_anchor.or(held);
            if let Some(focused) = held {
                ctx.memory_mut(|memory| memory.surrender_focus(focused));
            }
        }
        self.file_dialog_keys = self.file_job.is_some();
        if self.file_job.is_none() {
            return;
        }
        // The window itself is drawn later in the frame, after the navigation
        // layer has begun: a row drawn before `Nav::begin` is wiped from the
        // layout the arrows read, and the listing cannot be walked. Its answer
        // is acted on there too, still before the wall is drawn. See
        // `XgViewApp::frame` and `XgViewApp::draw_file_picker`.
        //
        // It is down now, so the control behind it takes the focus back - the
        // way it would have had the dialog not been opened at all. An import
        // that turned out to want the passphrase anchors itself instead.
        if self.file_job.is_none() && !self.passphrase.is_open() {
            if let Some(anchor) = self.file_dialog_anchor.take() {
                self.restore_focus = Some(anchor);
            }
        }
    }

    /// What Android does instead of a file dialog, for each of the two jobs.
    ///
    /// The export goes into the public `Download/xgview` folder through the
    /// activity, as it always has: on Android 10 and later the shared volume is
    /// reached through `MediaStore`, which needs no permission and writes into
    /// that folder; before it, a plain file and the storage permission the
    /// activity asks for. A file manager and a transfer both see the result,
    /// which is what the public folder is for - and what the app's own folder
    /// on that volume is not.
    ///
    /// The import walks the same window the desktop does, opening on that
    /// folder. It is the only way in: a document another application put on the
    /// device may not be read by path since Android 11, so what the viewer can
    /// import is what this window can list.
    #[cfg(target_os = "android")]
    pub(super) fn ask_for_file(&mut self, job: FileJob) {
        // The press that answered the dialog, read again by the control it was
        // opened from. See [`Self::file_dialog_answered`].
        if self.file_dialog_answered {
            return;
        }
        match job {
            FileJob::Export(passphrase) => self.export_to_download(passphrase),
            FileJob::Import => self.import_with_picker(),
        }
    }

    /// Opens the viewer's own picker for an import.
    ///
    /// Every platform ends up here, the desk included: the listing is walked by
    /// the arrows, which is the only way a remote can walk one. The dialog it
    /// replaces needed no provider and no permission behind it, and neither
    /// does this.
    ///
    /// It starts where [`picker_start_and_places`] says: `Download/xgview` -
    /// the folder an export goes into - on Android, the viewer's home on a
    /// desktop, and elsewhere on Android when neither can be walked.
    ///
    /// Android asks two different things before that window is worth opening,
    /// and waits for both the same way:
    ///
    /// - up to Android 9 the public folder is reachable only with the runtime
    ///   storage permission, which puts up a dialog answered over the app - and
    ///   a window opened first listed the app's own folder and nothing of the
    ///   volume it was opened for, which is what the first press used to do;
    /// - from Android 11 the volume is filtered by owner, so a configuration
    ///   copied on from another machine is not in the listing at all unless
    ///   this app holds "all files access" - a switch in the system settings
    ///   rather than a dialog, which is why the press sends the viewer there
    ///   once and the window waits for the trip back.
    ///
    /// See [`Self::poll_storage_pending`].
    pub(super) fn import_with_picker(&mut self) {
        #[cfg(target_os = "android")]
        {
            // The same question is already out, holding a press of the
            // viewer's: a second one must not queue behind it.
            if self.storage_pending.is_some() {
                return;
            }
            // Asked, and the window waits to be opened on the answer. `false`
            // is the permission being held already - every Android from 10 on,
            // where `MediaStore` needs none - or being beyond asking: refused
            // once, or no activity to ask. Then what can be shown is shown.
            if !crate::android::storage_granted() && crate::android::request_storage() {
                self.wait_for_storage(StorageJob::Import, "toast-import-grant-storage");
                return;
            }
            // The files other applications put on the device. Asked once per
            // run: a viewer who does not want the grant still gets the window,
            // listing what this app itself wrote, on every press after this.
            if !crate::android::all_files_access() && !self.all_files_asked {
                self.all_files_asked = true;
                if crate::android::request_all_files_access() {
                    self.wait_for_storage(
                        StorageJob::ImportAllFiles,
                        "toast-import-grant-all-files",
                    );
                    return;
                }
            }
        }
        self.open_import_picker();
    }

    /// Remembers a press as waiting, and says that it is.
    #[cfg(target_os = "android")]
    fn wait_for_storage(&mut self, job: StorageJob, said: &str) {
        self.storage_pending = Some(PendingStorage {
            job,
            asked: Instant::now(),
            resumes: crate::android::resume_count(),
        });
        self.flash(monitor_i18n::tr(said), ToastKind::Info);
    }

    /// Goes ahead with the press the storage was asked for, the moment it is in
    /// hand.
    ///
    /// Both halves of the configuration pair ask the same question and wait the
    /// same way: the import is not put up, the export is not written, until the
    /// folder can be reached. Neither answer is visible here - the runtime
    /// permission's arrives in Java, and the system-settings switch reports
    /// nothing at all - so the question is asked again every frame.
    ///
    /// A wait ends three ways: the grant arrives, the viewer comes back without
    /// it, or [`STORAGE_WAIT`] / [`ALL_FILES_WAIT`] runs out with the app in the
    /// background. The second is the one that matters for the switch: nothing
    /// says "answered", so a resumption is the whole of the answer - and the
    /// press then says what is missing and opens what can be shown, rather than
    /// leaving the viewer pressing a button that does nothing.
    #[cfg(target_os = "android")]
    pub(super) fn poll_storage_pending(&mut self) {
        let Some(pending) = self.storage_pending.take() else {
            return;
        };
        let wanted = pending.asked.elapsed();
        let limit = match pending.job {
            StorageJob::ImportAllFiles => ALL_FILES_WAIT,
            _ => STORAGE_WAIT,
        };
        if crate::android::storage_granted() && crate::android::all_files_access() {
            match pending.job {
                StorageJob::Import | StorageJob::ImportAllFiles => self.open_import_picker(),
                StorageJob::Export(passphrase) => self.write_export_now(passphrase),
            }
        } else if wanted > limit || crate::android::resume_count() != pending.resumes {
            let said = match pending.job {
                StorageJob::Import => "toast-import-storage-missing",
                StorageJob::ImportAllFiles => "toast-import-all-files-missing",
                StorageJob::Export(_) => "toast-export-storage-missing",
            };
            self.flash(monitor_i18n::tr(said), ToastKind::Error);
            // The press still owes its window: what can be shown is shown.
            if !matches!(pending.job, StorageJob::Export(_)) {
                self.open_import_picker();
            }
        } else {
            self.storage_pending = Some(pending);
        }
    }

    /// Puts the import window up, on whatever [`picker_start_and_places`] says.
    fn open_import_picker(&mut self) {
        let (start, places) = picker_start_and_places();
        self.file_picker.open(super::file_picker::FilePicker::start(
            start,
            places,
            super::file_picker::Mode::Open,
            String::new(),
        ));
        self.file_job = Some(FileJob::Import);
    }

    /// Opens the picker to write a configuration out, on a desktop.
    ///
    /// The same window as an import, with the field and the button it grows at
    /// its foot: the folder on screen is where the file goes, and the name is
    /// the one the viewer leaves in the field. It starts in the same place an
    /// import does, so a file written here is one the import can walk to
    /// without leaving the window.
    ///
    /// Android has no such window for an export: its file goes into the public
    /// `Download/xgview` through the activity, which is a folder the picker
    /// cannot walk and a write that needs no permission. See
    /// [`Self::export_to_download`].
    #[cfg(not(target_os = "android"))]
    pub(super) fn export_with_picker(&mut self, passphrase: Option<String>) {
        let (start, places) = picker_start_and_places();
        let name = if passphrase.is_some() {
            monitor_core::EXPORT_FILE_NAME
        } else {
            monitor_core::CONFIG_FILE_NAME
        };
        self.file_picker.open(super::file_picker::FilePicker::start(
            start,
            places,
            super::file_picker::Mode::Save,
            name.to_owned(),
        ));
        self.file_job = Some(FileJob::Export(passphrase));
    }

    /// Writes the configuration out into the public `Download/xgview` folder:
    /// sealed with `passphrase`, or the JSON in clear when there is none.
    ///
    /// Android's export opens no window. The folder is one the app is not free
    /// to walk - from Android 10 on the shared volume is reached through
    /// `MediaStore`, which writes into that folder without a permission - so a
    /// listing has nothing to choose, and the name is the one the file's kind
    /// gives it.
    ///
    /// Before Android 10 that folder is a plain file written with
    /// `WRITE_EXTERNAL_STORAGE`, which is the same permission the import window
    /// needs and is asked for in the same way: the press asks, waits for the
    /// answer, and goes ahead on the frame it comes. An export that tried the
    /// write first was refused by the platform and told the viewer to press
    /// again, which is a press that had already been made. See
    /// [`Self::write_export_now`].
    #[cfg(target_os = "android")]
    pub(super) fn export_to_download(&mut self, passphrase: Option<String>) {
        // The same dialog is already up, holding a press of the viewer's.
        if self.storage_pending.is_some() {
            return;
        }
        if !crate::android::storage_granted() && crate::android::request_storage() {
            self.wait_for_storage(StorageJob::Export(passphrase), "toast-export-grant-storage");
            return;
        }
        // Nothing to wait for: the permission is held, or it cannot be asked
        // for any more. Either way the write is what answers the press - and
        // one that cannot be made says so.
        self.write_export_now(passphrase);
    }

    /// The write itself, once the folder can be reached.
    ///
    /// The sealing is handed to a thread of its own and the write follows it
    /// there; see [`Self::poll_sealing`].
    #[cfg(target_os = "android")]
    fn write_export_now(&mut self, passphrase: Option<String>) {
        let name = if passphrase.is_some() {
            monitor_core::EXPORT_FILE_NAME
        } else {
            monitor_core::CONFIG_FILE_NAME
        };
        match passphrase {
            Some(passphrase) => self.spawn_seal(passphrase, ExportTarget::Download(name)),
            None => match self.config.to_json() {
                Ok(written) => self.write_download_export(name, &written),
                Err(error) => self.flash(
                    monitor_i18n::tr_args("toast-export-failed", &[("error", error.to_string().into())]),
                    ToastKind::Error,
                ),
            },
        }
    }

    /// Writes what an export came to into the public `Download/xgview` folder
    /// under `name`.
    ///
    /// The write is the activity's - `MediaStore` from Android 10 on, a plain
    /// file into the public Downloads folder before that - because a
    /// `NativeActivity` has no storage of its own the viewer can reach. The
    /// Android 9 permission the plain file needs has been asked for before this
    /// is reached; see [`Self::export_to_download`]. See
    /// `MainActivity.writeDownloadFile`.
    #[cfg(target_os = "android")]
    pub(super) fn write_download_export(&mut self, name: &str, written: &str) {
        match crate::android::write_download(name, written) {
            Ok(crate::android::ExportOutcome::Written(path)) => self.flash(
                monitor_i18n::tr_args("toast-config-exported", &[("path", path.into())]),
                ToastKind::Info,
            ),
            // Android 9 only: the storage permission has just been asked for,
            // so the export is one press away rather than failed.
            Ok(crate::android::ExportOutcome::NeedsStoragePermission) => {
                self.flash(monitor_i18n::tr("toast-export-needs-storage"), ToastKind::Error)
            }
            Err(error) => self.flash(
                monitor_i18n::tr_args("toast-export-failed", &[("error", error.into())]),
                ToastKind::Error,
            ),
        }
    }

    /// Whether the file dialog is up. It owns the screen while it is: the
    /// pointer, the focus and the keyboard all belong to it.
    pub(super) fn file_dialog_open(&self) -> bool {
        self.file_job.is_some()
    }

    /// What the export / import pair does, where the export has a fixed folder
    /// and the import walks the same folder through the viewer's own window.
    #[cfg(target_os = "android")]
    pub(super) fn import_hint(&self) -> String {
        monitor_i18n::tr("about-import-hint-android")
    }

    /// Writes the configuration out sealed with `passphrase`, to a file the
    /// viewer has already chosen.
    ///
    /// Choosing it is the window [`Self::ask_for_file`] opens; this is what
    /// happens once its answer is in. Sealing is handed to a thread
    /// of its own and the file is written when the text is in - see
    /// [`Self::poll_sealing`] - while the JSON, which costs nothing to produce,
    /// goes out from here.
    pub(super) fn export_to(&mut self, path: &Path, passphrase: Option<&str>) {
        match passphrase {
            Some(passphrase) => {
                self.spawn_seal(passphrase.to_owned(), ExportTarget::File(path.to_path_buf()))
            }
            None => match self.config.to_json() {
                Ok(written) => self.write_export_to(path, &written),
                Err(error) => self.flash(
                    monitor_i18n::tr_args("toast-export-failed", &[("error", error.to_string().into())]),
                    ToastKind::Error,
                ),
            },
        }
    }

    /// Writes what an export came to into the file the picker chose, and says
    /// where it went.
    ///
    /// Every platform that can write the path uses this: on Android that is the
    /// app's own folder on the shared volume, which needs no permission; a
    /// desktop writes wherever the viewer pointed the window.
    pub(super) fn write_export_to(&mut self, path: &Path, written: &str) {
        match std::fs::write(path, written.as_bytes()) {
            Ok(()) => self.flash(
                monitor_i18n::tr_args(
                    "toast-config-exported",
                    &[("path", path.display().to_string().into())],
                ),
                ToastKind::Info,
            ),
            Err(error) => self.flash(monitor_i18n::tr_args("toast-export-failed", &[("error", error.to_string().into())]), ToastKind::Error),
        }
    }

    /// Reads a configuration file the viewer has chosen into the running one.
    ///
    /// A sealed document asks for its passphrase before anything is touched, and
    /// the prompt takes it from there; an older export - JSON, in clear - is
    /// imported on the spot, the way it always was. The file was chosen by the
    /// dialog [`Self::ask_for_file`] opens - on Android, on the devices that
    /// have one, by the system's picker instead.
    pub(super) fn import_from(&mut self, path: PathBuf) {
        let data = match std::fs::read_to_string(&path) {
            Ok(data) => data,
            Err(error) => {
                self.flash(monitor_i18n::tr_args("toast-import-failed", &[("error", error.to_string().into())]), ToastKind::Error);
                return;
            }
        };
        if monitor_core::sealed::is_sealed(&data) {
            self.passphrase.open(Passphrase::for_import(data, Some(path)));
            return;
        }
        match self.apply_import(&data) {
            Ok(count) => self.say_imported(count, &path),
            Err(error) => self.flash(monitor_i18n::tr_args("toast-import-failed", &[("error", error.into())]), ToastKind::Error),
        }
    }

    /// Applies a configuration that was read from somewhere - a file the
    /// viewer picked, a sealed one the prompt has just opened - and reports how
    /// many cameras came with it, for the toast the caller puts up.
    pub(super) fn apply_import(&mut self, data: &str) -> Result<usize, String> {
        let imported = AppConfig::from_json(data).map_err(|error| error.to_string())?;
        Ok(self.apply_import_config(imported))
    }

    /// Applies a configuration already read, and reports how many cameras came
    /// with it.
    ///
    /// The cameras replace this viewer's, and the result is written back over
    /// this installation's configuration on the next save. With the "only the
    /// cameras" box ticked nothing else is taken from it.
    pub(super) fn apply_import_config(&mut self, imported: AppConfig) -> usize {
        let count = imported.cameras.len();
        self.back_up_configuration();
        if self.import_cameras_only {
            // The cameras come with the groups they live in. An entry records
            // the group it belongs to, so bringing the cameras without the
            // groups they name would leave the ones from the other groups in
            // groups this installation does not have - and never drawn.
            // Everything that is not the camera arrangement - the display, the
            // sleep and blackout settings, the scan ranges - stays this
            // viewer's own.
            self.config.cameras = imported.cameras;
            self.config.groups = imported.groups;
            self.config.active_group = imported.active_group;
            self.load_group_view();
        } else {
            // Everything is taken from the document, but the path this
            // installation reads and writes stays its own - it is a field of
            // the app, not of the configuration. The wall follows the imported
            // layout, page and focus, so the scheduler is rebuilt the way
            // start-up builds it.
            self.config = imported;
            let (layout, page, focus) = {
                let view = self.config.group();
                (view.layout, view.page, view.focus)
            };
            self.scheduler = Scheduler::new(layout, page, focus, self.config.enabled_count());
        }
        self.config.normalize();
        self.needs_sync = true;
        self.mark_dirty();
        count
    }

    /// Keeps the configuration an import is about to replace, beside it.
    ///
    /// Importing is the one thing here that can throw work away - everything
    /// else edits the configuration already loaded - so the file it was read
    /// from is copied aside first. A copy that cannot be written is reported and
    /// the import goes ahead anyway: not being able to keep the old file is no
    /// reason to refuse the new one, but it is something to hear about.
    pub(super) fn back_up_configuration(&mut self) {
        let path = self.config_path.with_extension("json.bak");
        // A copy, not a save: the version this replaces is already kept by the
        // save that follows, and `save` would keep a copy of the copy too.
        match self.config.save_copy(&path) {
            Ok(()) => tracing::info!(
                target: "xgview::config",
                path = %path.display(),
                "kept the configuration this import replaces"
            ),
            Err(error) => self.flash(
                monitor_i18n::tr_args(
                    "toast-backup-failed",
                    &[("error", error.to_string().into())],
                ),
                ToastKind::Error,
            ),
        }
    }

    /// Hands a sealed document and a passphrase to a thread of its own.
    ///
    /// Nothing waits for it: stretching a passphrase is hundreds of thousands of
    /// rounds, and the render loop has a wall to draw. What comes back is taken
    /// by [`Self::poll_sealing`] on a later frame.
    pub(super) fn spawn_open(&mut self, data: String, passphrase: String, origin: PathBuf) {
        let (sender, result) = unbounded();
        std::thread::spawn(move || {
            // A wrong passphrase and a damaged document are one answer here, as
            // they are inside `sealed::open`: neither says anything the other
            // does not, and the prompt has one thing to say about both.
            let _ = sender.send(monitor_core::sealed::open(&data, &passphrase).map_err(|_| ()));
        });
        self.sealing = Some(Sealing::Open { result, origin });
        self.mark_dirty();
    }

    /// Hands an export's passphrase to a thread of its own, with where the text
    /// it comes to is to be written.
    pub(super) fn spawn_seal(&mut self, passphrase: String, target: ExportTarget) {
        let (sender, result) = unbounded();
        let config = self.config.clone();
        std::thread::spawn(move || {
            let _ = sender.send(
                monitor_core::sealed::seal(&config, &passphrase).map_err(|error| error.to_string()),
            );
        });
        self.sealing = Some(Sealing::Export { result, target });
        self.mark_dirty();
    }

    pub(super) fn poll_sealing(&mut self) {
        use crossbeam_channel::TryRecvError;

        let Some(sealing) = self.sealing.take() else {
            return;
        };
        match sealing {
            Sealing::Open { result, origin } => match result.try_recv() {
                Ok(Ok(config)) => {
                    let count = self.apply_import_config(config);
                    self.say_imported(count, &origin);
                    self.passphrase.close();
                    self.restore_focus = self.file_dialog_anchor.take().or(self.restore_focus);
                    self.mark_dirty();
                }
                // A wrong passphrase and a damaged document read the same way,
                // and it is the passphrase that is worth another try: the entry
                // stays, the window stays with this said under it, and a second
                // attempt is a second field's worth of typing.
                Ok(Err(())) | Err(TryRecvError::Disconnected) => {
                    if let Some(prompt) = self.passphrase.payload.as_mut() {
                        prompt.working = false;
                        prompt.error = Some(monitor_i18n::tr("prompt-passphrase-wrong"));
                    }
                }
                Err(TryRecvError::Empty) => {
                    self.sealing = Some(Sealing::Open { result, origin });
                }
            },
            Sealing::Export { result, target } => match result.try_recv() {
                Ok(Ok(written)) => match target {
                    ExportTarget::File(path) => self.write_export_to(&path, &written),
                    #[cfg(target_os = "android")]
                    ExportTarget::Download(name) => self.write_download_export(name, &written),
                },
                Ok(Err(error)) => self.flash(
                    monitor_i18n::tr_args("toast-export-failed", &[("error", error.into())]),
                    ToastKind::Error,
                ),
                // The thread is gone without an answer, which the seal itself
                // has no way of doing. The file is not written, and that is what
                // is said about it.
                Err(TryRecvError::Disconnected) => {
                    self.flash(monitor_i18n::tr("toast-export-interrupted"), ToastKind::Error)
                }
                Err(TryRecvError::Empty) => {
                    self.sealing = Some(Sealing::Export { result, target });
                }
            },
        }
    }

    /// Says what an import of `count` cameras did, naming the file it came
    /// from: every import on Android is the viewer's own window choosing a row,
    /// so there is always a path to name.
    #[cfg(target_os = "android")]
    pub(super) fn say_imported(&mut self, count: usize, origin: &Path) {
        self.flash(
            monitor_i18n::tr_args(
                "toast-imported",
                &[("count", count.into()), ("path", origin.display().to_string().into())],
            ),
            ToastKind::Info,
        );
    }
}

/// Where the picker opens, and what it offers above the listing.
///
/// Android is answered by [`android_start_and_places`], which knows about the
/// public `Download` folder an export goes into. Everywhere else it is two
/// entries, never a wall of them: the home folder and the volumes the machine
/// has. The per-user folders a native dialog also lists - desktop, documents,
/// downloads, pictures and the rest - are all inside home, and a remote walks
/// this list one press at a time; a shortcut for each of them is a list to read
/// rather than one to remember.
fn picker_start_and_places() -> (PathBuf, Vec<(String, PathBuf)>) {
    #[cfg(target_os = "android")]
    {
        android_start_and_places()
    }
    #[cfg(not(target_os = "android"))]
    {
        let home = home_dir();
        let start = start_dir(home.as_deref());
        let mut places = Vec::new();
        if let Some(home) = &home {
            places.push((monitor_i18n::tr("file-picker-place-home"), home.clone()));
        }
        places.extend(device_places());
        (start, places)
    }
}

/// The folder an export is written into inside the public one.
///
/// The second half of `MainActivity.EXPORT_FOLDER`, which is where the activity
/// puts the file; spelled here because the picker opens on it.
pub const EXPORT_SUBFOLDER: &str = "xgview";

/// The public `Download` folder on the volume the app's own folder sits on.
///
/// `<volume>/Android/data/<package>/files` names the volume, and an export goes
/// to `<volume>/Download/xgview`. Derived rather than handed over: the entry
/// point hands out the app's own folder, and a third string from Java would be
/// a third string to keep in step. `None` when the path has no `Android`
/// segment, which nothing on a device has.
#[cfg(any(target_os = "android", test))]
pub fn public_download_dir(external_files: &Path) -> Option<PathBuf> {
    let volume = external_files
        .ancestors()
        .find(|dir| dir.ends_with("Android"))
        .and_then(Path::parent)
        // A bare `Android` has nothing above it to join a `Download` to, and a
        // relative answer would be a folder nobody meant.
        .filter(|volume| volume.components().next().is_some())?;
    Some(volume.join("Download"))
}

/// Whether the picker could walk `dir` at all.
///
/// `is_dir` is not the question: `Download` on a device whose app holds no
/// storage permission answers that and then refuses the listing, which is a
/// window that opens on an error the viewer can do nothing about.
#[cfg(any(target_os = "android", test))]
fn readable(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok()
}

/// What the picker opens on and offers, on Android.
///
/// `Download/xgview` first: it is where [`XgViewApp::export_to_download`] puts
/// an export, so the file the viewer made a moment ago is the first thing an
/// import shows. Two fallbacks, in order - the public `Download` itself, which
/// is all a device that has never exported has, and the app's own folder, the
/// one place always readable and writable and the only one a device with no
/// storage permission shows anything in.
#[cfg(target_os = "android")]
fn android_start_and_places() -> (PathBuf, Vec<(String, PathBuf)>) {
    let own = crate::android::external_files_dir();
    let export = crate::android::export_folder().filter(|dir| readable(dir));
    let download = crate::android::download_dir().filter(|dir| readable(dir));
    let start = export
        .clone()
        .or_else(|| download.clone())
        .or_else(|| own.clone())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let mut places = Vec::new();
    if let Some(own) = own {
        places.push((monitor_i18n::tr("file-picker-place-files"), own));
    }
    if let Some(download) = export.or(download) {
        places.push((monitor_i18n::tr("file-picker-place-downloads"), download));
    }
    (start, places)
}

/// The folder the picker opens on, where home is the only place to name.
#[cfg(not(target_os = "android"))]
fn start_dir(home: Option<&Path>) -> PathBuf {
    home.map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The viewer's home folder, on a desktop.
///
/// A television does not have one in any sense its viewer would recognise; what
/// it opens on is [`android_start_and_places`]'s business.
#[cfg(not(target_os = "android"))]
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from).filter(|dir| dir.is_dir())
}

/// The volumes above the listing.
///
/// Windows names them by drive letter. `GetLogicalDrives` is asked rather than
/// each letter's `is_dir`: a disconnected network drive blocks on the filesystem
/// until the client gives up, and that would be the frame the window opens on.
#[cfg(windows)]
fn device_places() -> Vec<(String, PathBuf)> {
    let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    let mut places = Vec::new();
    // A: and B: are the legacy floppy positions; no machine that runs this has
    // one, and asking about them can stall a controller that is still mapped.
    for index in 2..26u32 {
        if mask & (1 << index) == 0 {
            continue;
        }
        let letter = (b'A' + index as u8) as char;
        places.push((format!("{letter}:"), PathBuf::from(format!("{letter}:\\"))));
    }
    places
}

/// Linux names its volumes by where they are mounted.
///
/// The block devices in `/dev` are nodes, not folders - `read_dir` on one fails -
/// so the picker offers the places they are mounted at: the root, the homes, and
/// whatever removable medium or manual mount is under `/media`, `/mnt` or
/// `/run/media`.
#[cfg(target_os = "linux")]
fn device_places() -> Vec<(String, PathBuf)> {
    let mut places = Vec::new();
    for root in ["/", "/home"] {
        let path = PathBuf::from(root);
        if path.is_dir() {
            places.push((root.to_owned(), path));
        }
    }
    for base in ["/media", "/mnt", "/run/media"] {
        let Ok(read) = std::fs::read_dir(base) else { continue };
        for item in read.flatten() {
            let path = item.path();
            if !item.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            // `/run/media` holds one folder per user; the mounts are inside it.
            if base == "/run/media" {
                if let Ok(inner) = std::fs::read_dir(&path) {
                    for mount in inner.flatten() {
                        if mount.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                            places.push((
                                mount.file_name().to_string_lossy().into_owned(),
                                mount.path(),
                            ));
                        }
                    }
                }
                continue;
            }
            places.push((item.file_name().to_string_lossy().into_owned(), path));
        }
    }
    places
}

/// macOS names its volumes by their labels, under `/Volumes`.
#[cfg(target_os = "macos")]
fn device_places() -> Vec<(String, PathBuf)> {
    let mut places = vec![("/".to_owned(), PathBuf::from("/"))];
    if let Ok(read) = std::fs::read_dir("/Volumes") {
        for item in read.flatten() {
            if item.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                places.push((item.file_name().to_string_lossy().into_owned(), item.path()));
            }
        }
    }
    places
}

/// Android is not asked: what it offers is [`android_start_and_places`]'s, and
/// a device has no volume a listing could walk to.
#[cfg(not(any(windows, target_os = "linux", target_os = "macos", target_os = "android")))]
fn device_places() -> Vec<(String, PathBuf)> {
    Vec::new()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// A remote that walked down to `D:\` has no `..` above it; the only way
    /// onto another volume is a shortcut to its root.
    #[test]
    fn every_volume_on_the_machine_is_one_press_away() {
        for (label, path) in device_places() {
            // A volume need not be readable - an optical drive with no disc in
            // it still has a letter, and the list names it rather than probing
            // every root on the frame the window opens. What matters is that
            // every entry is a drive root and nothing else.
            assert_eq!(label.len(), 2, "{label} is a drive letter");
            assert!(path.is_absolute(), "{label} is not a path");
            assert_eq!(path.parent(), None, "{label} is a drive root");
        }
    }

    /// The window opens on home, and home is the first thing in the list.
    #[test]
    fn the_picker_opens_on_home_with_home_in_the_list() {
        let (start, places) = picker_start_and_places();
        let home = home_dir().expect("a Windows session always names a profile");
        assert_eq!(start, home);
        assert_eq!(places.first().map(|(_, path)| path.clone()), Some(home));
    }
}

#[cfg(test)]
mod download_tests {
    use super::*;

    /// `Download` is found from the app's own folder rather than handed over,
    /// so it has to come out right for every shape that folder takes: the
    /// emulated volume, the older `/sdcard` spelling, and a card.
    #[test]
    fn the_public_folder_sits_beside_the_apps_own() {
        for (own, expected) in [
            (
                "/storage/emulated/0/Android/data/com.xhbl.xgview/files",
                "/storage/emulated/0/Download",
            ),
            (
                "/storage/emulated/10/Android/data/com.xhbl.xgview/files",
                "/storage/emulated/10/Download",
            ),
            ("/sdcard/Android/data/com.xhbl.xgview/files", "/sdcard/Download"),
            (
                "/storage/1234-ABCD/Android/data/com.xhbl.xgview/files",
                "/storage/1234-ABCD/Download",
            ),
        ] {
            assert_eq!(
                public_download_dir(Path::new(own)),
                Some(PathBuf::from(expected)),
                "{own}"
            );
        }
    }

    /// A folder with no `Android` above it is not on a volume the app was given
    /// a folder on, and nothing is guessed for it.
    #[test]
    fn a_folder_that_is_not_under_android_names_no_downloads() {
        assert_eq!(public_download_dir(Path::new("/home/xgview")), None);
        assert_eq!(public_download_dir(Path::new("Android")), None);
    }

    /// A place is offered only when the picker could walk it: `Download` on a
    /// device with no storage permission answers `is_dir` and then refuses the
    /// listing, which would be a window opening on an error.
    #[test]
    fn a_folder_that_cannot_be_listed_is_not_a_place() {
        let dir = std::env::temp_dir().join("xgview-picker-readable");
        let _ = std::fs::create_dir_all(&dir);
        assert!(readable(&dir));
        assert!(!readable(&dir.join("not-there")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
