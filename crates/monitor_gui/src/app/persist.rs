//! The configuration on disk: the debounced save, the export and the import,
//! and the file dialog they go through.

use super::*;

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

    /// Opens what Android has instead of a file dialog.
    ///
    /// The export writes through the viewer's own picker, into the folder it
    /// walks to - the app's own on the shared volume, which needs no
    /// permission. The import is handed to the system's document picker when
    /// the device has one, and to that same window when it does not.
    #[cfg(target_os = "android")]
    pub(super) fn ask_for_file(&mut self, job: FileJob) {
        // The press that answered the dialog, read again by the control it was
        // opened from. See [`Self::file_dialog_answered`].
        if self.file_dialog_answered {
            return;
        }
        match job {
            // The export now writes where the viewer points it, through the
            // same window the import uses: a remote can walk one picker, and
            // the app's own folder on the shared volume is writable without a
            // permission. See [`Self::export_with_picker`].
            FileJob::Export(passphrase) => self.export_with_picker(passphrase),
            FileJob::Import => {
                // A television build can ship without `DocumentsUI` and without
                // a single document provider behind it, while still answering
                // the intent with the framework's placeholder - so asking
                // whether there is a picker is not the same question as asking
                // whether anything claims to be one. See
                // `MainActivity.hasDocumentPicker`.
                if crate::android::has_document_picker() {
                    self.import_config();
                } else {
                    self.import_with_picker();
                }
            }
        }
    }

    /// Opens the viewer's own picker for an import.
    ///
    /// Every platform that has no document picker to hand an import to ends up
    /// here - a television build that ships without `DocumentsUI` above all -
    /// and so does the desktop: the listing is walked by the arrows, which is
    /// the only way a remote can walk one. The dialog it replaces needed no
    /// provider and no permission behind it, and neither does this.
    ///
    /// It starts where [`start_dir`] says: the app's own folder on the shared
    /// volume on Android, the viewer's home on a desktop. Both are places a
    /// file transfer can put a configuration without a permission behind it.
    pub(super) fn import_with_picker(&mut self) {
        let (start, places) = picker_start_and_places();
        self.file_picker.open(super::file_picker::FilePicker::start(
            start,
            places,
            super::file_picker::Mode::Open,
            String::new(),
        ));
        self.file_job = Some(FileJob::Import);
    }

    /// Opens the picker to write a configuration out.
    ///
    /// The same window as an import, with the field and the button it grows at
    /// its foot: the folder on screen is where the file goes, and the name is
    /// the one the viewer leaves in the field. It starts in the same place an
    /// import does, so a file written here is one the import can walk to
    /// without leaving the window.
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

    /// Whether the file dialog is up. It owns the screen while it is: the
    /// pointer, the focus and the keyboard all belong to it.
    pub(super) fn file_dialog_open(&self) -> bool {
        self.file_job.is_some()
    }

    /// What the export / import pair does, where the export has a fixed folder
    /// and the import opens the system's document picker.
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
            Ok(count) => self.say_imported(count, Some(path)),
            Err(error) => self.flash(monitor_i18n::tr_args("toast-import-failed", &[("error", error.into())]), ToastKind::Error),
        }
    }

    /// Opens the system's document picker.
    ///
    /// Nothing is imported here: the picker is another activity, and its answer
    /// arrives frames later, where [`Self::poll_picked_config`] takes it. A
    /// picker the viewer backs out of is not a failure and says nothing.
    #[cfg(target_os = "android")]
    pub(super) fn import_config(&mut self) {
        crate::android::pick_config_file();
    }

    /// Applies a configuration that was read from somewhere - a file the
    /// desktop picked, a document the Android picker returned, a sealed one the
    /// prompt has just opened - and reports how many cameras came with it, for
    /// the toast the caller puts up.
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

    /// Takes the document the system picker returned, if one has arrived.
    ///
    /// A sealed document asks for its passphrase before anything is touched; an
    /// older export, JSON in clear, is imported on the spot.
    #[cfg(target_os = "android")]
    pub(super) fn poll_picked_config(&mut self) {
        match crate::android::take_picked_config() {
            None => {}
            Some(crate::android::Picked::Cancelled) => {}
            Some(crate::android::Picked::Config(text)) => {
                if monitor_core::sealed::is_sealed(&text) {
                    self.passphrase.open(Passphrase::for_import(text, None));
                    return;
                }
                match self.apply_import(&text) {
                    Ok(count) => self.say_imported(count, None),
                    Err(error) => self.flash(monitor_i18n::tr_args("toast-import-failed", &[("error", error.into())]), ToastKind::Error),
                }
            }
            Some(crate::android::Picked::Failed(error)) => {
                self.flash(monitor_i18n::tr_args("toast-import-failed", &[("error", error.into())]), ToastKind::Error)
            }
        }
    }

    /// Hands a sealed document and a passphrase to a thread of its own.
    ///
    /// Nothing waits for it: stretching a passphrase is hundreds of thousands of
    /// rounds, and the render loop has a wall to draw. What comes back is taken
    /// by [`Self::poll_sealing`] on a later frame.
    pub(super) fn spawn_open(&mut self, data: String, passphrase: String, origin: Option<PathBuf>) {
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
                    self.say_imported(count, origin);
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

    /// Says what an import of `count` cameras did, the picked document having no
    /// path to name.
    #[cfg(target_os = "android")]
    pub(super) fn say_imported(&mut self, count: usize, _origin: Option<PathBuf>) {
        self.flash(
            monitor_i18n::tr_args("toast-imported-document", &[("count", count.into())]),
            ToastKind::Info,
        );
    }
}

/// Where the picker opens, and what it offers above the listing.
///
/// Two entries, never a wall of them: the home folder and the volumes the
/// machine has. The per-user folders a native dialog also lists - desktop,
/// documents, downloads, pictures and the rest - are all inside home, and a
/// remote walks this list one press at a time; a shortcut for each of them is
/// a list to read rather than one to remember.
fn picker_start_and_places() -> (PathBuf, Vec<(String, PathBuf)>) {
    let home = home_dir();
    let start = start_dir(home.as_deref());
    let mut places = Vec::new();
    if let Some(home) = &home {
        places.push((monitor_i18n::tr("file-picker-place-home"), home.clone()));
    }
    places.extend(device_places());
    (start, places)
}

/// The folder the picker opens on.
///
/// Android opens on the app's own folder on the shared volume: since Android 11
/// a program sees nothing of that volume but the folders it made, and this is
/// the one a file transfer can put a configuration into without a permission
/// behind it. The desktops open on home, which is where a browser's download
/// lands and where the exports carried between machines are kept.
fn start_dir(home: Option<&Path>) -> PathBuf {
    home.map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The viewer's home folder, when the platform names one.
///
/// A television does not have one in any sense its viewer would recognise: the
/// only folder it may read is the app's own on the shared volume, so that is
/// what is called home here - the window opens on it and the list offers it,
/// exactly as a desktop does with `~`.
fn home_dir() -> Option<PathBuf> {
    #[cfg(target_os = "android")]
    {
        crate::android::external_files_dir()
    }
    #[cfg(not(target_os = "android"))]
    {
        #[cfg(windows)]
        let home = std::env::var_os("USERPROFILE");
        #[cfg(not(windows))]
        let home = std::env::var_os("HOME");
        home.map(PathBuf::from).filter(|dir| dir.is_dir())
    }
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

/// Android has one folder it may read, and that is where the window opens.
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
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
