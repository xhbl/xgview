//! The viewer's own file picker: a listing that a remote control can walk.
//!
//! The dialog this replaces is a mouse's: its rows are drawn allocations with
//! no focus model behind them, so a television remote cannot move onto one, and
//! a device without a document provider has no way to import at all. This one
//! is built the way the rest of the interface is - every row a registered
//! control of a column scope, so Up and Down walk it, Enter opens a directory
//! or takes a file, and Back steps out of the directory it is in before it
//! closes the window.
//!
//! It answers both questions the settings page asks. An import walks the
//! listing and takes a file; an export walks the same listing, names the file
//! in a field of its own and writes it into the folder on screen. One window,
//! so a remote has one set of keys to learn, and the save it replaces on the
//! desktop - a native dialog - was the one screen a remote could not reach at
//! all.

use super::*;

use std::fs;
use std::path::{Path, PathBuf};

/// The scope the rows live in.
const LIST_SCOPE: &'static str = "file-picker-list";
/// The scope the shortcuts above the listing live in.
const PLACES_SCOPE: &'static str = "file-picker-places";
/// The scope the save field and its button live in.
const ACTIONS_SCOPE: &'static str = "file-picker-actions";

/// Height of one row.
///
/// The space a television can spare is not the space a phone has: this window
/// has to fit a handset held sideways, whose whole height is under 400 points.
/// A row is still a hit target for an arrow rather than a finger, so it only
/// has to hold one line of text - see [`LIST_ROWS`] for the rest of the budget.
const ROW_H: f32 = 32.0;
/// How many rows the listing shows before it scrolls.
///
/// The window's height is measured out rather than grown to fit: it is fixed,
/// so a folder with two files looks the same as one with two hundred, and the
/// arrows never make it jump. How much room there is to measure out depends on
/// the screen, and a handset held sideways is the tight one - 384 points on the
/// 2340x1080 phone this was measured on. Title, path, shortcuts, the name field,
/// the hint and the frame come to about 214 of them, which leaves four rows and
/// a margin on either side. Five rows is 374 - it fits, but it touches both
/// edges, which is the thing the camera form already does. A desktop is not
/// short of height, so it shows eight.
const LIST_ROWS: usize = if cfg!(target_os = "android") { 4 } else { 8 };
/// The extensions a configuration comes in: the JSON written in clear, and the
/// sealed export. Both have to be listed - the settings page writes one or the
/// other, and a picker that only showed the first could not read back what this
/// very program had just written. The later dialog accepted both; this one has
/// to as well.
const SUFFIXES: [&str; 2] = [".json", ".seal"];

/// One row of the listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Entry {
    /// The way back up. Never present at a root.
    Parent,
    /// The way out of a root that has nothing to show. A listing always keeps
    /// at least one row: a scope with no members drops the focus, and the
    /// arrows go back to the wall while the window is still up.
    Close,
    /// A directory: Enter walks into it.
    Directory { name: String, path: PathBuf },
    /// A file this picker accepts: Enter takes it.
    File { name: String, path: PathBuf, size: u64 },
}

/// What the window is showing: one directory, read from disk.
#[derive(Debug, Clone, Default)]
pub(super) struct Listing {
    /// The directory the rows were read from.
    dir: PathBuf,
    /// Its rows, in the order they are drawn.
    entries: Vec<Entry>,
    /// Why it could not be read, when it could not.
    error: Option<String>,
}

impl Listing {
    /// Reads one directory into rows.
    ///
    /// Directories first, then the files this picker takes, each group in
    /// case-insensitive name order: a remote walks a list one press at a time,
    /// and a list in the order the filesystem happens to hand over is one the
    /// viewer has to read rather than remember. Hidden entries - a name that
    /// starts with a dot - are left out: on Android and Linux they are the
    /// plumbing of the directory, not something an import is ever after.
    fn read(dir: &Path) -> Self {
        let mut entries = Vec::new();
        let mut error = None;
        match fs::read_dir(dir) {
            Ok(read) => {
                let mut dirs: Vec<(String, PathBuf)> = Vec::new();
                let mut files: Vec<(String, PathBuf, u64)> = Vec::new();
                for item in read.flatten() {
                    let name = item.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        continue;
                    }
                    let Ok(kind) = item.file_type() else { continue };
                    let path = item.path();
                    if kind.is_dir() {
                        dirs.push((name, path));
                    } else if kind.is_file() && takes(&name) {
                        let size = item.metadata().map(|meta| meta.len()).unwrap_or(0);
                        files.push((name, path, size));
                    }
                }
                dirs.sort_by_key(|(name, _)| name.to_lowercase());
                files.sort_by_key(|(name, _, _)| name.to_lowercase());
                if dir.parent().is_some() {
                    entries.push(Entry::Parent);
                }
                entries.extend(dirs.into_iter().map(|(name, path)| Entry::Directory { name, path }));
                entries.extend(files.into_iter().map(|(name, path, size)| Entry::File { name, path, size }));
            }
            Err(err) => error = Some(err.to_string()),
        }
        // A directory that cannot be read - or a root with nothing in it -
        // still offers one row to walk onto. Without it the focus has no
        // widget, egui drops it, and the remote is left pressing arrows at the
        // wall behind a window that is still up.
        if entries.is_empty() {
            entries.push(placeholder(dir));
        }
        Self { dir: dir.to_path_buf(), entries, error }
    }

    /// The directory above this one, or `None` at a root.
    fn above(&self) -> Option<PathBuf> {
        self.dir.parent().map(Path::to_path_buf)
    }
}

/// The one row a listing that has nothing else can offer.
fn placeholder(dir: &Path) -> Entry {
    if dir.parent().is_some() { Entry::Parent } else { Entry::Close }
}

/// Whether a file's name is one this picker takes.
fn takes(name: &str) -> bool {
    let lower = name.to_lowercase();
    SUFFIXES.iter().any(|suffix| lower.ends_with(suffix))
}

/// A file's size, short enough to sit at the end of a row.
fn size_label(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// What the window was opened for.
///
/// The listing is the same either way; what differs is what the confirm key and
/// the button at the foot of the window do with the row under the focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Take a file that is already there.
    Open,
    /// Write one into the folder on screen, named in the field at the foot of
    /// it. What it is sealed under belongs to the job that opened the window,
    /// not to the listing.
    Save,
}

/// The picker's window: where it is and what it offers as shortcuts.
#[derive(Debug)]
pub(super) struct FilePicker {
    mode: Mode,
    listing: Listing,
    /// Folders offered above the listing, labelled by the caller - it knows
    /// which ones exist on this platform.
    places: Vec<(String, PathBuf)>,
    /// The name a save writes, editable in the field at the foot of the window.
    name: String,
    /// A directory was just left or entered: put the focus back on the first
    /// row of the new listing. The rows of the old one are gone, and egui
    /// drops a focus whose widget is no longer drawn - which would leave the
    /// window up with the arrows going to the wall again.
    reanchor: bool,
}

impl Default for FilePicker {
    fn default() -> Self {
        Self { mode: Mode::Open, listing: Listing::default(), places: Vec::new(), name: String::new(), reanchor: false }
    }
}

impl FilePicker {
    /// Opens the picker on `dir` with `places` as its shortcuts.
    pub(super) fn start(
        dir: PathBuf,
        places: Vec<(String, PathBuf)>,
        mode: Mode,
        name: String,
    ) -> Self {
        Self { mode, listing: Listing::read(&dir), places, name, reanchor: false }
    }

    /// Whether this window writes a file rather than taking one.
    fn saving(&self) -> bool {
        self.mode == Mode::Save
    }

    /// The file a save would write, or `None` while the name cannot be one.
    ///
    /// A name with a separator in it is refused rather than followed: the field
    /// is a name, not a path, and `../` there would write outside the folder the
    /// listing is showing - which the viewer cannot see. The extensions the
    /// listing accepts are not forced on it; the name arrives with the one the
    /// export wants.
    fn target(&self) -> Option<PathBuf> {
        let name = self.name.trim();
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains(['/', '\\'])
            || name.contains('\0')
        {
            return None;
        }
        Some(self.listing.dir.join(name))
    }

    /// Moves up one directory, and says whether there was one to move to.
    ///
    /// The window closes on `false`: Back steps out of the listing first, so a
    /// window opened three directories deep is not closed by one press.
    pub(super) fn up(&mut self) -> bool {
        let Some(above) = self.listing.above() else {
            return false;
        };
        self.listing = Listing::read(&above);
        self.reanchor = true;
        true
    }

    /// Draws the window. `Answer::CLOSE` means it is going down: the caller
    /// reads [`take_picked`](Self::take_picked) and [`took_back`](Self::took_back)
    /// to find out why.
    pub(super) fn body(&mut self, app: &mut XgViewApp, ui: &mut egui::Ui) -> Answer {
        // Escape, or Back at a root, asked to leave. The request is carried on
        // the app rather than acted on in `handle_keys`: closing through the
        // window's own answer is what lets `draw_file_picker` clear `file_job`,
        // the flag that hands the keyboard back to the rest of the interface.
        if app.file_picker_cancelled {
            return Answer::CLOSE;
        }
        // One action per frame at most, applied after the rows are drawn: a
        // press that changes the directory while the rows are being walked
        // would leave the loop reading a listing that no longer exists.
        let mut go: Option<PathBuf> = None;
        let mut take: Option<PathBuf> = None;
        let mut rename: Option<String> = None;
        let mut close = false;

        let title = if self.saving() { "file-picker-title-save" } else { "file-picker-title" };
        let bar = dialogs::title_bar(ui, &monitor_i18n::tr(title), true, &monitor_i18n::tr("dialog-close"));
        if let Some(button) = bar.close {
            if app.nav.tracked(button).clicked() {
                close = true;
            }
        }

        // Where the viewer is, in full: a listing without its path is a wall of
        // names with no way to tell one folder from another.
        let path = self.listing.dir.display().to_string();
        ui.label(RichText::new(path).small().color(theme::TEXT_DIM));

        if !self.places.is_empty() {
            ui.add_space(theme::space::S);
            app.nav.open(PLACES_SCOPE);
            ui.horizontal_wrapped(|ui| {
                for (label, path) in &self.places {
                    let button = ui.button(label);
                    let button = app.nav.tracked(button);
                    if button.clicked() {
                        go = Some(path.clone());
                    }
                }
            });
            app.nav.close();
        }

        ui.add_space(theme::space::S);

        app.nav.open(LIST_SCOPE);
        // A row is a hand-drawn allocation rather than a `Button`, and egui
        // only turns Enter into a click for the widgets it implements itself -
        // so the confirm key is read here and given to the row that has the
        // focus. Read once for the frame, and only a fresh press: a held key
        // would otherwise walk into a folder and out of it again.
        let confirm = ui.input(|input| {
            input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Key {
                        key: Key::Enter | Key::Space,
                        pressed: true,
                        repeat: false,
                        ..
                    }
                )
            })
        });
        // The height is fixed, not a maximum: a `max_height` alone lets a folder
        // with two files shrink the window to two rows, and the window would then
        // change size as the viewer walks from one directory to the next.
        // `min_scrolled_height` is what holds the space open, `auto_shrink` stops
        // the scroll area from narrowing to its content, and the rows themselves
        // are sized exactly to [`ROW_H`] rather than by their text.
        let listing_h = ROW_H * LIST_ROWS as f32;
        let listing = egui::ScrollArea::vertical()
            .max_height(listing_h)
            .min_scrolled_height(listing_h)
            .auto_shrink([false, false]);
        listing.show(ui, |ui| {
            // Rows touch. egui puts its item spacing between two allocations,
            // and on six rows that is most of a row's worth of height - which a
            // window that has to fit a handset held sideways cannot spare. What
            // separates one row from the next is the focus ring and the hover
            // fill, not a gap.
            ui.spacing_mut().item_spacing.y = 0.0;
            // Said inside the listing rather than above it: a line outside the
            // scroll area is a line the window grows by, and walking into a
            // folder that cannot be read would resize the window - which is
            // exactly what a fixed height is not. Here the line scrolls with
            // the rows, and the window stays the size it was.
            if let Some(error) = &self.listing.error {
                ui.label(RichText::new(error).color(theme::ERROR));
            } else if self
                .listing
                .entries
                .iter()
                .all(|entry| matches!(entry, Entry::Parent | Entry::Close))
            {
                ui.label(
                    RichText::new(monitor_i18n::tr("file-picker-empty")).color(theme::TEXT_DIM),
                );
            }
            for (seat, entry) in self.listing.entries.iter().enumerate() {
                let (rect, response) =
                    ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                let response = app.nav.tracked(response);
                // The status line looks this name up as a translation key, so
                // every row has to name a key that exists in both bundles.
                app.name(
                    &response,
                    match entry {
                        Entry::Parent => "file-picker-parent",
                        Entry::Close => "dialog-close",
                        Entry::Directory { .. } => "file-picker-folder",
                        Entry::File { .. } => "file-picker-file",
                    },
                );
                // The window's anchor, not the picker's: `Modal::hand_off` is
                // what gives the first row the focus as the window goes up, and
                // it reads the modal's field. Written here, while the payload is
                // out of the modal and this window is being drawn - the frame
                // the hand-off happens on.
                if seat == 0 {
                    app.file_picker.anchor = Some(response.id);
                    if self.reanchor {
                        ui.ctx().memory_mut(|memory| memory.request_focus(response.id));
                        self.reanchor = false;
                    }
                }
                if ui.is_rect_visible(rect) {
                    let visuals = ui.style().interact_selectable(&response, false);
                    let corner = CornerRadius::same(theme::radius::S);
                    if response.hovered() || response.has_focus() {
                        ui.painter().rect_filled(rect, corner, visuals.weak_bg_fill);
                    }
                    if visuals.bg_stroke.width > 0.0 {
                        ui.painter().rect_stroke(
                            rect,
                            corner,
                            visuals.bg_stroke,
                            StrokeKind::Inside,
                        );
                    }
                    let (text, tail) = match entry {
                        Entry::Parent => (monitor_i18n::tr("file-picker-parent"), String::new()),
                        Entry::Close => (monitor_i18n::tr("dialog-close"), String::new()),
                        Entry::Directory { name, .. } => (format!("{name}/"), String::new()),
                        Entry::File { name, size, .. } => (name.clone(), size_label(*size)),
                    };
                    let color = match entry {
                        Entry::Directory { .. } | Entry::Parent | Entry::Close => theme::ACCENT,
                        Entry::File { .. } => visuals.text_color(),
                    };
                    ui.painter().text(
                        egui::pos2(rect.left() + 10.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        text,
                        FontId::proportional(15.0),
                        color,
                    );
                    if !tail.is_empty() {
                        ui.painter().text(
                            egui::pos2(rect.right() - 10.0, rect.center().y),
                            Align2::RIGHT_CENTER,
                            tail,
                            FontId::proportional(12.0),
                            theme::TEXT_DIM,
                        );
                    }
                }
                if response.clicked() || (confirm && response.has_focus()) {
                    match entry {
                        Entry::Parent => go = self.listing.above(),
                        Entry::Close => close = true,
                        Entry::Directory { path, .. } => go = Some(path.clone()),
                        // A save writes a name the field below carries, so an
                        // existing file is not something to open: a press fills
                        // the name in, so the next save replaces it. The confirm
                        // key is left for the footer, where it saves.
                        Entry::File { name, path, .. } => {
                            if self.saving() {
                                if response.clicked() {
                                    rename = Some(name.clone());
                                }
                            } else {
                                take = Some(path.clone());
                            }
                        }
                    }
                }
            }
        });
        app.nav.close();

        if let Some(name) = rename {
            self.name = name;
        }

        // A save names its file before it is written: the field and its button
        // are the footer of the window, and the folder they write into is the
        // one the listing above is showing.
        if self.saving() {
            ui.add_space(theme::space::S);
            ui.separator();
            ui.add_space(theme::space::XS);
            app.nav.open(ACTIONS_SCOPE);
            ui.horizontal(|ui| {
                ui.label(monitor_i18n::tr("file-picker-name"));
                let field = dialogs::text_field(
                    ui,
                    &mut app.nav,
                    &mut self.name,
                    &monitor_i18n::tr("file-picker-name"),
                    false,
                    false,
                    |text| egui::TextEdit::singleline(text).desired_width(240.0),
                );
                app.name(&field, "file-picker-name");
                // Only what can be a file name enables the button: an empty
                // field, or one with a separator in it, is not a path this
                // window may write.
                let target = self.target();
                let save = ui.add_enabled(
                    target.is_some(),
                    egui::Button::new(monitor_i18n::tr("file-picker-save")),
                );
                let save = app.nav.tracked(save);
                app.name(&save, "file-picker-save");
                // Enter confirms from the field; a space is a character in a
                // name and is read as one, so only Enter saves from here.
                let enter = ui.input(|input| {
                    input.events.iter().any(|event| {
                        matches!(
                            event,
                            egui::Event::Key { key: Key::Enter, pressed: true, repeat: false, .. }
                        )
                    })
                });
                if save.clicked() || (enter && field.has_focus()) {
                    take = target;
                }
            });
            app.nav.close();
        }

        ui.add_space(theme::space::S);
        // A television has no Esc: its BACK is the key that walks up a level.
        // Say that, rather than promising a key the remote does not have.
        let hint = match (self.saving(), cfg!(target_os = "android")) {
            (true, true) => "file-picker-hint-save-android",
            (true, false) => "file-picker-hint-save",
            (false, true) => "file-picker-hint-android",
            (false, false) => "file-picker-hint",
        };
        ui.label(RichText::new(monitor_i18n::tr(hint)).small().color(theme::TEXT_DIM));

        if let Some(dir) = go {
            self.listing = Listing::read(&dir);
            self.reanchor = true;
        }
        if let Some(path) = take {
            app.file_picker_picked = Some(path);
            return Answer::CLOSE;
        }
        if close {
            app.file_picker_cancelled = true;
            return Answer::CLOSE;
        }
        Answer::STAY
    }
}

impl XgViewApp {
    /// Draws the import picker while it is up, and acts on what it answered.
    ///
    /// The answer is read from the two fields the window wrote rather than from
    /// its payload: [`modal_frame`](XgViewApp::modal_frame) drops the payload as
    /// it closes, which is the same frame the answer arrives on.
    pub(super) fn draw_file_picker(&mut self, ctx: &egui::Context) {
        let answer = self.modal_frame(
            ctx,
            "xgview-file-picker",
            560.0,
            |app| &mut app.file_picker,
            |app, ui, picker| picker.body(app, ui),
        );
        if answer.keep {
            return;
        }
        let picked = self.file_picker_picked.take();
        let cancelled = std::mem::take(&mut self.file_picker_cancelled);
        let job = self.file_job.take();
        self.file_dialog_keys = false;
        self.file_dialog_answered = true;
        // The window is drawn after `poll_file_dialog`, so the restore at the
        // end of that function never sees `file_job` go away. The button the
        // window was asked from takes the focus back here instead.
        if !self.passphrase.is_open() {
            self.restore_focus = self.file_dialog_anchor.take().or(self.restore_focus);
        }
        if let Some(path) = picked {
            // One window answers both directions, so which one it was opened
            // for is what decides what the path means: a document to read, or
            // the file an export is to be written into.
            match job {
                Some(FileJob::Export(passphrase)) => self.export_to(&path, passphrase.as_deref()),
                Some(FileJob::Import) | None => self.import_from(path),
            }
        } else if cancelled {
            // Backed out of: nothing is imported or exported, and there is
            // nothing to say about it.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xgview-picker-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn directories_come_before_files_and_each_group_is_sorted() {
        let dir = scratch("order");
        fs::create_dir(dir.join("zeta")).unwrap();
        fs::create_dir(dir.join("Alpha")).unwrap();
        fs::write(dir.join("b-config.json"), b"{}").unwrap();
        fs::write(dir.join("A-config.json"), b"{}").unwrap();
        fs::write(dir.join("notes.txt"), b"x").unwrap();

        let listing = Listing::read(&dir);
        let names: Vec<String> = listing
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Parent | Entry::Close => None,
                Entry::Directory { name, .. } | Entry::File { name, .. } => Some(name.clone()),
            })
            .collect();
        // Case-insensitive order, directories first, and the text file - which
        // no configuration can come from - left out entirely.
        assert_eq!(names, ["Alpha", "zeta", "A-config.json", "b-config.json"]);
    }

    #[test]
    fn hidden_entries_are_left_out() {
        let dir = scratch("hidden");
        fs::write(dir.join(".secret.json"), b"{}").unwrap();
        fs::write(dir.join("shown.json"), b"{}").unwrap();
        let listing = Listing::read(&dir);
        let files: Vec<&String> = listing
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::File { name, .. } => Some(name),
                _ => None,
            })
            .collect();
        assert_eq!(files, ["shown.json"]);
    }

    #[test]
    fn the_way_up_appears_except_at_a_root() {
        let dir = scratch("root");
        let listing = Listing::read(&dir);
        assert_eq!(listing.entries.first(), Some(&Entry::Parent), "a temporary folder has a parent");
        assert_eq!(listing.above(), dir.parent().map(Path::to_path_buf));

        let root = Path::new("/");
        let listing = Listing::read(root);
        assert!(!listing.entries.contains(&Entry::Parent), "the root has nowhere to go up to");
        assert!(listing.above().is_none());
    }

    #[test]
    fn a_directory_that_cannot_be_read_says_so_instead_of_showing_nothing() {
        let missing = std::env::temp_dir().join("xgview-picker-does-not-exist");
        let listing = Listing::read(&missing);
        assert!(listing.error.is_some());
        // It still has a row for the focus to land on. An empty listing used to
        // leave the navigation scope with no members: the focus was dropped and
        // the arrows went to the wall while the window stayed up.
        assert_eq!(listing.entries, [Entry::Parent]);
    }

    #[test]
    fn a_root_with_nothing_to_show_still_offers_a_way_out() {
        let root = PathBuf::from(std::path::MAIN_SEPARATOR.to_string());
        assert_eq!(placeholder(&root), Entry::Close, "a root has no way up");
        assert_eq!(placeholder(&root.join("somewhere")), Entry::Parent);
    }

    #[test]
    fn sizes_are_short_enough_for_the_end_of_a_row() {
        assert_eq!(size_label(512), "512 B");
        assert_eq!(size_label(2048), "2 KB");
        assert_eq!(size_label(3 * 1024 * 1024 / 2), "1.5 MB");
    }

    #[test]
    fn both_kinds_of_configuration_are_listed() {
        assert!(takes("config.json"), "an export written in clear");
        assert!(takes("xgvconfig.seal"), "a sealed export, which the settings page writes");
        assert!(takes("CONFIG.JSON"));
        assert!(takes("XGVCONFIG.SEAL"));
        assert!(!takes("config.json.bak"));
        assert!(!takes("config"));
        assert!(!takes("notes.txt"));
    }

    #[test]
    fn a_save_writes_a_name_inside_the_folder_on_screen() {
        let dir = scratch("save-target");
        let picker =
            FilePicker::start(dir.clone(), Vec::new(), Mode::Save, "xgvconfig.seal".to_owned());
        assert_eq!(picker.target(), Some(dir.join("xgvconfig.seal")));

        // The field is a name, not a path: a separator in it would write
        // outside the folder the listing is showing, which the viewer cannot
        // see. An empty or dot-only name is not a file either.
        for name in ["", "  ", ".", "..", "sub/other.json", "sub\\other.json", "bad\0name"] {
            let picker = FilePicker::start(dir.clone(), Vec::new(), Mode::Save, name.to_owned());
            assert!(picker.target().is_none(), "{name:?} must not be written as a path");
        }
    }

    #[test]
    fn an_open_window_has_no_file_to_write() {
        let dir = scratch("open-target");
        let picker = FilePicker::start(dir, Vec::new(), Mode::Open, "xgvconfig.json".to_owned());
        assert!(!picker.saving());
    }
}
