//! The viewer's own file picker: a listing that a remote control can walk.
//!
//! The dialog this replaces is a mouse's: its rows are drawn allocations with
//! no focus model behind them, so a television remote cannot move onto one, and
//! a device without a document provider has no way to import at all. This one
//! is built the way the rest of the interface is - every row a registered
//! control of a column scope, so Up and Down walk it, Enter opens a directory
//! or takes a file, and Back closes the window. The way up is a button beside
//! the shortcuts, not a key.
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
/// The scope the name field lives in.
const FIELD_SCOPE: &'static str = "file-picker-field";
/// The scope the button beside the field lives in.
///
/// Beside, and a scope of its own, because a text field keeps Left and Right
/// for its caret - so the button would have no key that reaches it if the two
/// shared a row. Down out of the field enters this scope, and Up leaves it.
const BUTTON_SCOPE: &'static str = "file-picker-buttons";

/// The way up's label. A glyph rather than a word, so it reads as a control of
/// its own beside the folder shortcuts and costs a fraction of their width.
const UP_ICON: &str = "↑";
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
/// 2340x1080 phone this was measured on. Title, path, shortcuts, the name field
/// and the frame come to about 180 of them once the way up is a button and the
/// instructions are gone, which leaves five rows and a margin on either side.
/// A desktop is not short of height, so it shows eight.
const LIST_ROWS: usize = if cfg!(target_os = "android") { 5 } else { 8 };
/// The extensions a configuration comes in: the JSON written in clear, and the
/// sealed export. An import has to list both - the settings page writes one or
/// the other, and a picker that only showed the first could not read back what
/// this very program had just written. The later dialog accepted both; this one
/// has to as well.
const SUFFIXES: [&str; 2] = [".json", ".seal"];
/// What a save lists: only the kind it is about to write.
///
/// The listing is not a catalogue of everything readable, it is the set of
/// files the viewer can name this export after. Showing a sealed file in a
/// clear export - or the other way round - invites the very mistake the
/// extension is there to prevent: one press would put a name of the wrong kind
/// in the field, and the file it replaces would have been written as the other.
const CLEAR_SUFFIXES: [&str; 1] = [".json"];
const SEALED_SUFFIXES: [&str; 1] = [".seal"];

/// One row of the listing.
///
/// Going up is not a row: it is a button beside the shortcuts - see
/// [`FilePicker::body`] - because a row for it costs the height of a folder in
/// every directory, and Back is the one key that leaves the window now. A
/// folder with nothing to show has no rows at all; the wall says so in words
/// and the focus stays on the shortcuts above, which are the only things there
/// are to press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Entry {
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
    fn read(dir: &Path, mode: Mode) -> Self {
        let mut entries = Vec::new();
        let mut error = None;
        let suffixes = suffixes(mode);
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
                    } else if kind.is_file() && takes(&name, suffixes) {
                        let size = item.metadata().map(|meta| meta.len()).unwrap_or(0);
                        files.push((name, path, size));
                    }
                }
                dirs.sort_by_key(|(name, _)| name.to_lowercase());
                files.sort_by_key(|(name, _, _)| name.to_lowercase());
                entries.extend(dirs.into_iter().map(|(name, path)| Entry::Directory { name, path }));
                entries.extend(files.into_iter().map(|(name, path, size)| Entry::File { name, path, size }));
            }
            Err(err) => error = Some(err.to_string()),
        }
        Self { dir: dir.to_path_buf(), entries, error }
    }

    /// The directory above this one, or `None` at a root.
    fn above(&self) -> Option<PathBuf> {
        self.dir.parent().map(Path::to_path_buf)
    }
}

/// Whether a walk into a folder should hand the ring to the listing's first
/// row. A press on the up button keeps it on that button while the button is
/// still enabled; any other way in - and a button that has just gone grey -
/// hands it down instead.
fn focuses_listing_after_walk(from_up: bool, up_still_enabled: bool) -> bool {
    !from_up || !up_still_enabled
}

/// The window's navigation scopes, declared beside the window they walk.
///
/// Down from the shortcuts enters the listing; Down from the listing enters the
/// name field; Down from the field enters the button beside it, and Up unwinds
/// the same way. The field and the button are two scopes rather than one row
/// because a text field keeps Left and Right for its caret - in a row of its
/// own the button beside it would have no key that reaches it at all, which is
/// exactly how the Save button used to be unreachable from a remote.
///
/// An import draws no footer, so the last two scopes have no members then and
/// their exits lead nowhere: Up out of the listing still leaves for the
/// shortcuts.
pub(super) fn scopes() -> [(&'static str, ScopeDef); 4] {
    [
        (PLACES_SCOPE, ScopeDef::new(Axis::Row).exit(Dir::Down, Exit::Scope(LIST_SCOPE))),
        (LIST_SCOPE, ScopeDef::new(Axis::Column).scrolling()
            .exit(Dir::Up, Exit::Scope(PLACES_SCOPE))
            .exit(Dir::Down, Exit::Scope(FIELD_SCOPE))),
        (FIELD_SCOPE, ScopeDef::new(Axis::Row)
            .exit(Dir::Up, Exit::Scope(LIST_SCOPE))
            .exit(Dir::Down, Exit::Scope(BUTTON_SCOPE))),
        (BUTTON_SCOPE, ScopeDef::new(Axis::Row).exit(Dir::Up, Exit::Scope(FIELD_SCOPE))),
    ]
}

/// The extensions this window lists, given what it was opened for.
///
/// An import reads both kinds. A save lists only the kind it is about to write,
/// so a name taken from a row cannot quietly change an export from clear to
/// sealed or back - and so a file of the other kind cannot be replaced by one.
fn suffixes(mode: Mode) -> &'static [&'static str] {
    match mode {
        Mode::Open => &SUFFIXES,
        Mode::Save { sealed: false } => &CLEAR_SUFFIXES,
        Mode::Save { sealed: true } => &SEALED_SUFFIXES,
    }
}

/// Whether a file's name is one this window lists.
fn takes(name: &str, suffixes: &[&str]) -> bool {
    let lower = name.to_lowercase();
    suffixes.iter().any(|suffix| lower.ends_with(suffix))
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
/// The rows it lists and what the confirm key does with the row under the focus
/// both come from this: an import reads either kind of configuration, a save
/// shows only the kind it is writing - clear or sealed - and names its file in
/// the field at the foot of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Take a file that is already there, clear or sealed.
    Open,
    /// Write one into the folder on screen, named in the field at the foot of
    /// it. `sealed` is whether a passphrase was given for this export, which
    /// is also the kind of file the listing is filtered to. The passphrase
    /// itself belongs to the job that opened the window, not to the picker.
    ///
    /// Android's export goes straight into the public Download folder via the
    /// activity, so the only mode it ever constructs is `Open` - the field,
    /// the filter and the overwrite question are a desktop window's.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    Save { sealed: bool },
}

/// What a press on the Save button came to. See [`FilePicker::press`].
#[derive(Debug, PartialEq, Eq)]
enum SavePress {
    /// The field does not name a file this window may write.
    Nothing,
    /// The name is taken, and the footer is asking whether to replace it.
    Asking,
    /// Write it.
    Write(PathBuf),
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
    /// The name the footer is asking about, while it is asking.
    ///
    /// A save whose name is already in the folder writes nothing on the press
    /// that asked for it: the field makes way for the question, and only the
    /// answer writes. `None` when nothing is being asked - which is every
    /// window until a save lands on a name that is taken.
    ask: Option<String>,
    /// The question has just gone up: the answer that changes nothing takes
    /// the focus as it is drawn.
    ask_focus: bool,
    /// The question was answered with "no": the field comes back and takes the
    /// focus, so the viewer is naming a file again rather than looking at a
    /// window with no ring in it.
    refocus_field: bool,
}

impl Default for FilePicker {
    fn default() -> Self {
        Self {
            mode: Mode::Open,
            listing: Listing::default(),
            places: Vec::new(),
            name: String::new(),
            reanchor: false,
            ask: None,
            ask_focus: false,
            refocus_field: false,
        }
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
        Self {
            mode,
            listing: Listing::read(&dir, mode),
            places,
            name,
            reanchor: false,
            ask: None,
            ask_focus: false,
            refocus_field: false,
        }
    }

    /// Whether this window writes a file rather than taking one.
    fn saving(&self) -> bool {
        matches!(self.mode, Mode::Save { .. })
    }

    /// Whether the name carries the extension of the *other* kind of export.
    ///
    /// The kind is decided by the checkbox on the settings page, not by what
    /// the field says, so a clear export named `.seal` would put clear text in
    /// a sealed-looking file - and the other way round. Refused rather than
    /// silently corrected: the field is the viewer's to name.
    fn wrong_kind(&self) -> bool {
        let Mode::Save { sealed } = self.mode else {
            return false;
        };
        let name = self.name.trim().to_lowercase();
        if sealed {
            name.ends_with(".json")
        } else {
            name.ends_with(".seal")
        }
    }

    /// The file a save would write, or `None` while the name cannot be one.
    ///
    /// A name with a separator in it is refused rather than followed: the field
    /// is a name, not a path, and `../` there would write outside the folder the
    /// listing is showing - which the viewer cannot see. The extensions the
    /// listing accepts are not forced on it; the name arrives with the one the
    /// export wants. A name that says the *other* kind is refused too; see
    /// [`Self::wrong_kind`].
    fn target(&self) -> Option<PathBuf> {
        let name = self.name.trim();
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains(['/', '\\'])
            || name.contains('\0')
            || self.wrong_kind()
        {
            return None;
        }
        Some(self.listing.dir.join(name))
    }

    /// Enters `dir` after a press, and decides where the focus should land.
    ///
    /// `from_up` is whether the press was on the button beside the shortcuts.
    /// That button is still there in the folder above - a viewer climbing
    /// several levels presses the same control again - so its focus ring stays
    /// on it rather than being handed to the first row of the new listing. The
    /// one exception is the button having gone grey: at a root there is no
    /// further up to go, and a ring on a disabled control answers nothing.
    /// Every other way into a folder re-anchors as it always did.
    fn walk(&mut self, dir: &Path, from_up: bool) {
        self.listing = Listing::read(dir, self.mode);
        self.reanchor = focuses_listing_after_walk(from_up, self.listing.above().is_some());
        // A question about a name in the folder just left is a question about
        // a file that is no longer on screen.
        self.ask = None;
    }

    /// What a press on the Save button comes to.
    ///
    /// A name that is free is written on the press. One that is already in the
    /// folder is not: an export is the one thing this program writes, and a
    /// file it wrote an hour ago is as likely to be the thing the viewer means
    /// to keep as it is to be the one being replaced - so the footer asks, and
    /// the answer is what writes. The question is about the name in the field
    /// as it stood when the press was made; see [`Self::sync_ask`].
    fn press(&mut self, target: Option<PathBuf>) -> SavePress {
        let Some(path) = target else {
            return SavePress::Nothing;
        };
        let name = self.name.trim().to_owned();
        // Already asked about this name: the press that reaches here a second
        // time is the one that answers "yes", which is answered at the buttons
        // instead - so wait rather than ask again.
        if self.ask.as_deref() == Some(name.as_str()) {
            return SavePress::Asking;
        }
        if !path.exists() {
            return SavePress::Write(path);
        }
        self.ask = Some(name);
        self.ask_focus = true;
        SavePress::Asking
    }

    /// Drops the question when the field no longer names what it asked about.
    ///
    /// The field is drawn again as soon as a question stops being asked, and a
    /// viewer who types a different name there has answered the question by
    /// meaning something else.
    fn sync_ask(&mut self) {
        if self.ask.as_deref().is_some_and(|asked| asked != self.name.trim()) {
            self.ask = None;
        }
    }

    /// Drops the question, saying whether there was one to drop.
    ///
    /// Back is how a viewer says "not this one" to a question they did not open
    /// a window for: it takes the question down and leaves the listing where it
    /// is, rather than stepping out of the folder underneath it.
    pub(super) fn dismiss_ask(&mut self) -> bool {
        if self.ask.take().is_none() {
            return false;
        }
        self.refocus_field = true;
        true
    }

    /// Draws the window. `Answer::CLOSE` means it is going down: the caller
    /// reads [`take_picked`](Self::take_picked) and [`took_back`](Self::took_back)
    /// to find out why.
    pub(super) fn body(&mut self, app: &mut XgViewApp, ui: &mut egui::Ui) -> Answer {
        // A Back key asked to leave. The request is carried on
        // the app rather than acted on in `handle_keys`: closing through the
        // window's own answer is what lets `draw_file_picker` clear `file_job`,
        // the flag that hands the keyboard back to the rest of the interface.
        if app.file_picker_cancelled {
            return Answer::CLOSE;
        }
        // One action per frame at most, applied after the rows are drawn: a
        // press that changes the directory while the rows are being walked
        // would leave the loop reading a listing that no longer exists.
        // Where a press takes the listing, and whether that press was the up
        // button - which is the one that keeps its focus ring (see `walk`).
        let mut go: Option<(PathBuf, bool)> = None;
        let mut take: Option<PathBuf> = None;
        let mut write: Option<PathBuf> = None;
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

        // The shortcuts, with the way up beside them.
        //
        // Going up used to be the first row of the listing, and then a Back
        // press. Both cost too much now: a row takes the height of a folder in
        // every directory, and Back closes the window on every platform. So
        // this button is the one way up - a pointer has no keys at all - and it
        // is greyed at a root, where up is nowhere.
        let above = self.listing.above();
        let mut up: Option<(Id, bool)> = None;
        // Where the focus goes when the listing has no rows at all - a folder
        // that is empty or cannot be read. The shortcuts above it are then the
        // only controls in the window, and the home folder is the one a viewer
        // would reach for.
        let mut first: Option<Id> = None;
        ui.add_space(theme::space::S);
        app.nav.open(PLACES_SCOPE);
        ui.horizontal_wrapped(|ui| {
            for (label, path) in &self.places {
                let button = ui.button(label);
                let button = app.nav.tracked(button);
                if first.is_none() {
                    first = Some(button.id);
                }
                if button.clicked() {
                    go = Some((path.clone(), false));
                }
            }
            let button = ui.add_enabled(above.is_some(), egui::Button::new(UP_ICON));
            let button = app.nav.tracked(button);
            if first.is_none() {
                first = Some(button.id);
            }
            // What the status line calls it. A control whose label is a glyph
            // has nothing for the line to read otherwise.
            app.name(&button, "file-picker-up");
            if button.clicked() {
                if let Some(parent) = &above {
                    // The ring follows the button that was pressed: climbing
                    // several levels is pressing this same control again, and
                    // `walk` leaves the ring here while it is still enabled.
                    // A mouse press is included - it may have come from a row,
                    // and the button is what the viewer just chose.
                    ui.ctx().memory_mut(|memory| memory.request_focus(button.id));
                    go = Some((parent.clone(), true));
                }
            }
            up = Some((button.id, above.is_none()));
        });
        app.nav.close();

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
                // A folder that cannot be read is worth saying so in either
                // mode: a save into it is as impossible as an import from it.
                ui.label(RichText::new(error).color(theme::ERROR));
            } else if self.listing.entries.is_empty() && !self.saving() {
                // "No configuration here" is news to somebody *looking* for a
                // file. A save is here to create one, so an empty folder is
                // exactly what it wants and the line would only read as a
                // complaint about the state of things. The empty space is kept
                // by the scroll area, and the footer below says what to do.
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
                    // A button that has just gone grey - the viewer walked back
                    // up to a root - is no place for the ring: it answers
                    // nothing now, and the listing's first row is what it would
                    // have led to. Handed on in the same frame, on the row it
                    // goes to, because that row is the one just drawn.
                    if let Some((id, true)) = up {
                        if ui.ctx().memory(|memory| memory.focused()) == Some(id) {
                            ui.ctx().memory_mut(|memory| memory.request_focus(response.id));
                        }
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
                        Entry::Directory { name, .. } => (format!("{name}/"), String::new()),
                        Entry::File { name, size, .. } => (name.clone(), size_label(*size)),
                    };
                    let color = match entry {
                        Entry::Directory { .. } => theme::ACCENT,
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
                        Entry::Directory { path, .. } => go = Some((path.clone(), false)),
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

        // A folder with nothing to show has no rows, and a scope with no
        // members is where a focus goes to die - the arrows would go back to
        // the wall with this window still up, and the window would open with
        // nothing in it focused at all. The shortcuts above are what is left,
        // so the window opens on the first of them, the home folder, and
        // stepping into an empty folder hands the focus there too.
        if self.listing.entries.is_empty() {
            app.file_picker.anchor = first;
            if self.reanchor {
                if let Some(id) = first {
                    ui.ctx().memory_mut(|memory| memory.request_focus(id));
                }
                self.reanchor = false;
            }
        }

        if let Some(name) = rename {
            self.name = name;
        }

        // A save names its file before it is written: the field and its button
        // are the footer of the window, and the folder they write into is the
        // one the listing above is showing.
        //
        // A name that is already in that folder turns the footer into a
        // question, which takes the field's place rather than growing the
        // window: the window is measured out once - see [`LIST_ROWS`] - and a
        // row that came and went would move everything above it.
        if self.saving() {
            ui.add_space(theme::space::S);
            ui.separator();
            ui.add_space(theme::space::XS);
            self.sync_ask();
            let asking = self.ask.clone();
            ui.horizontal(|ui| {
                if let Some(asked) = &asking {
                    // A name can be longer than the footer: truncated, so the
                    // two answers stay inside the window rather than being
                    // pushed off its right edge.
                    ui.add(
                        egui::Label::new(
                            RichText::new(monitor_i18n::tr_args(
                                "file-picker-overwrite",
                                &[("name", asked.as_str().into())],
                            ))
                            .small()
                            .color(theme::TEXT_DIM),
                        )
                        .truncate(),
                    );
                    app.nav.open(BUTTON_SCOPE);
                    let overwrite = ui.button(RichText::new(monitor_i18n::tr("file-picker-overwrite-yes")));
                    let overwrite = app.nav.tracked(overwrite);
                    app.name(&overwrite, "file-picker-overwrite-yes");
                    let cancel = ui.button(monitor_i18n::tr("action-cancel"));
                    let cancel = app.nav.tracked(cancel);
                    app.name(&cancel, "action-cancel");
                    // The question opens on the answer that changes nothing,
                    // the way the reboot prompt does. Both buttons are drawn by
                    // now, and the focus asked for here is read next frame -
                    // the same seam `reanchor` uses.
                    if std::mem::take(&mut self.ask_focus) {
                        ui.ctx().memory_mut(|memory| memory.request_focus(cancel.id));
                    }
                    if overwrite.clicked() {
                        // Answered: the name is being replaced on purpose.
                        write = self.target();
                    }
                    if cancel.clicked() {
                        self.dismiss_ask();
                    }
                    app.nav.close();
                } else {
                    // The kind is written beside the field rather than left for
                    // the extension to imply: the listing above is filtered to
                    // it, and the name about to be written goes out as this
                    // kind whatever the field ends up saying.
                    let sealed = matches!(self.mode, Mode::Save { sealed: true });
                    let kind_key = if sealed { "file-picker-kind-sealed" } else { "file-picker-kind-clear" };
                    let kind = monitor_i18n::tr(kind_key);
                    let label = if self.wrong_kind() {
                        RichText::new(monitor_i18n::tr_args(
                            "file-picker-kind-mismatch",
                            &[("kind", kind.as_str().into())],
                        ))
                        .color(theme::ERROR)
                    } else {
                        RichText::new(format!("{} · {}", monitor_i18n::tr("file-picker-name"), kind))
                    };
                    ui.label(label);
                    app.nav.open(FIELD_SCOPE);
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
                    if std::mem::take(&mut self.refocus_field) {
                        ui.ctx().memory_mut(|memory| memory.request_focus(field.id));
                    }
                    app.nav.close();
                    app.nav.open(BUTTON_SCOPE);
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
                    // Enter confirms from the field; a space is a character in
                    // a name and is read as one, so only Enter saves from here.
                    let enter = ui.input(|input| {
                        input.events.iter().any(|event| {
                            matches!(
                                event,
                                egui::Event::Key { key: Key::Enter, pressed: true, repeat: false, .. }
                            )
                        })
                    });
                    if save.clicked() || (enter && field.has_focus()) {
                        if let SavePress::Write(path) = self.press(target) {
                            write = Some(path);
                        }
                    }
                    app.nav.close();
                }
            });
        }

        // There is no line of instructions under the window. The keys are the
        // same ones the rest of the interface already answers - arrows to walk,
        // OK to confirm, Back to close - and on a handset held sideways a
        // line of them costs height the listing can spend on a folder.

        if let Some((dir, from_up)) = go {
            self.walk(&dir, from_up);
        }
        if let Some(path) = take.or(write) {
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
        // A path is an answer only while the window has one. The export's is
        // the name the field held when Save was pressed - already asked about,
        // if there was something in the folder under it - and the import's is
        // the row the focus was on.
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

        let listing = Listing::read(&dir, Mode::Open);
        let names: Vec<String> = listing
            .entries
            .iter()
            .filter_map(|entry| match entry {
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
        let listing = Listing::read(&dir, Mode::Open);
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

    /// Going up is not a row any more - it is the button beside the shortcuts -
    /// so what a listing has to answer is only whether there is anywhere to go
    /// up to. That is what greys the button out at a root.
    #[test]
    fn the_way_up_is_known_except_at_a_root() {
        let dir = scratch("root");
        let listing = Listing::read(&dir, Mode::Open);
        assert_eq!(listing.above(), dir.parent().map(Path::to_path_buf));
        // Nothing in it, so no rows - and no row for going up either: that is
        // the button. The window puts the focus on its shortcuts then; see
        // `FilePicker::body`.
        assert!(listing.entries.is_empty());

        let root = Path::new("/");
        let listing = Listing::read(root, Mode::Open);
        assert!(listing.above().is_none(), "the root has nowhere to go up to");
    }

    #[test]
    fn a_directory_that_cannot_be_read_says_so_instead_of_showing_nothing() {
        let missing = std::env::temp_dir().join("xgview-picker-does-not-exist");
        let listing = Listing::read(&missing, Mode::Open);
        assert!(listing.error.is_some());
        // No rows at all: what the wall says instead is a line of text, and the
        // focus is handed to the shortcuts above the listing. A row that only
        // closed the window was there before, and it was the strangest thing in
        // the picker - a folder's worth of height saying "Close".
        assert!(listing.entries.is_empty());
    }

    #[test]
    fn sizes_are_short_enough_for_the_end_of_a_row() {
        assert_eq!(size_label(512), "512 B");
        assert_eq!(size_label(2048), "2 KB");
        assert_eq!(size_label(3 * 1024 * 1024 / 2), "1.5 MB");
    }

    #[test]
    fn an_import_lists_both_kinds_of_configuration_and_nothing_else() {
        let all = suffixes(Mode::Open);
        assert!(takes("config.json", all), "an export written in clear");
        assert!(takes("xgvconfig.seal", all), "a sealed export, which the settings page writes");
        assert!(takes("CONFIG.JSON", all));
        assert!(takes("XGVCONFIG.SEAL", all));
        assert!(!takes("config.json.bak", all));
        assert!(!takes("config", all));
        assert!(!takes("notes.txt", all));
    }

    /// The filter follows the export: a clear export lists JSON only, a sealed
    /// one lists `.seal` only, and switching folders keeps the filter. A file of
    /// the other kind is not a name this export may be given from a row, and is
    /// not a file this export can replace by accident.
    #[test]
    fn a_save_lists_only_the_kind_it_writes() {
        let dir = scratch("save-filter");
        fs::create_dir(dir.join("nested")).unwrap();
        fs::write(dir.join("clear.json"), b"{}").unwrap();
        fs::write(dir.join("CLEAR-UPPER.JSON"), b"{}").unwrap();
        fs::write(dir.join("sealed.seal"), b"{}").unwrap();
        fs::write(dir.join("notes.txt"), b"x").unwrap();

        fn files(listing: &Listing) -> Vec<String> {
            listing.entries.iter().filter_map(|entry| match entry {
                Entry::File { name, .. } => Some(name.clone()),
                _ => None,
            }).collect()
        }

        assert_eq!(files(&Listing::read(&dir, Mode::Open)).len(), 3, "an import reads both kinds");
        assert_eq!(
            files(&Listing::read(&dir, Mode::Save { sealed: false })),
            ["CLEAR-UPPER.JSON", "clear.json"],
            "a clear export sees JSON only"
        );
        assert_eq!(
            files(&Listing::read(&dir, Mode::Save { sealed: true })),
            ["sealed.seal"],
            "a sealed export sees .seal only"
        );
        // Directories stay visible in every mode - they are where the export
        // may be written instead.
        assert!(Listing::read(&dir, Mode::Save { sealed: true })
            .entries
            .iter()
            .any(|entry| matches!(entry, Entry::Directory { .. })));

        // Walking into a folder re-reads with the very same filter.
        let mut picker = FilePicker::start(
            dir.clone(),
            Vec::new(),
            Mode::Save { sealed: true },
            "xgvconfig.seal".to_owned(),
        );
        assert_eq!(files(&picker.listing), ["sealed.seal"]);
        picker.listing = Listing::read(&dir.join("nested"), Mode::Save { sealed: true });
        picker.walk(&dir, true);
        assert_eq!(files(&picker.listing), ["sealed.seal"]);
    }

    #[test]
    fn a_save_writes_a_name_inside_the_folder_on_screen() {
        let dir = scratch("save-target");
        let picker =
            FilePicker::start(dir.clone(), Vec::new(), Mode::Save { sealed: true }, "xgvconfig.seal".to_owned());
        assert_eq!(picker.target(), Some(dir.join("xgvconfig.seal")));

        // The field is a name, not a path: a separator in it would write
        // outside the folder the listing is showing, which the viewer cannot
        // see. An empty or dot-only name is not a file either.
        for name in ["", "  ", ".", "..", "sub/other.json", "sub\\other.json", "bad\0name", "xgvconfig.json"] {
            let picker = FilePicker::start(dir.clone(), Vec::new(), Mode::Save { sealed: true }, name.to_owned());
            assert!(picker.target().is_none(), "{name:?} must not be written as a path or the wrong kind");
        }
    }

    /// The extension of the other kind is refused in both directions, while a
    /// name with no recognised extension is still the viewer's to choose.
    #[test]
    fn a_save_refuses_the_other_kinds_extension() {
        let dir = scratch("kind-mismatch");
        let clear = |name: &str| {
            FilePicker::start(dir.clone(), Vec::new(), Mode::Save { sealed: false }, name.to_owned())
        };
        let sealed = |name: &str| {
            FilePicker::start(dir.clone(), Vec::new(), Mode::Save { sealed: true }, name.to_owned())
        };

        assert!(clear("backup.seal").target().is_none(), "clear text in a .seal file");
        assert!(sealed("backup.json").target().is_none(), "a sealed blob in a .json file");
        assert!(clear("BACKUP.SEAL").target().is_none(), "case does not matter");
        assert_eq!(clear("backup.json").target(), Some(dir.join("backup.json")));
        assert_eq!(sealed("backup.seal").target(), Some(dir.join("backup.seal")));
        assert_eq!(clear("backup").target(), Some(dir.join("backup")), "a custom name is allowed");
        // An import has no kind to disagree with.
        let open = FilePicker::start(dir, Vec::new(), Mode::Open, "backup.seal".to_owned());
        assert!(!open.wrong_kind());
    }

    /// Down walks the footer: shortcuts, listing, name field, and the button
    /// beside it - which is the press that used to have no key at all, because
    /// a field and a button in one row leaves the button behind the field's
    /// caret keys.
    ///
    /// Driven off the very scopes the window is drawn under, so the walk this
    /// asserts is the walk the application gets.
    #[test]
    fn down_walks_the_footer_and_up_unwinds_it() {
        use crate::nav::{Dir, Nav};

        let (place, row, field, save) = (
            Id::new("place"),
            Id::new("row"),
            Id::new("field"),
            Id::new("save"),
        );
        let mut nav = Nav::new(&scopes());
        let ctx = egui::Context::default();

        // One frame of the window: a shortcut, one row, the field and Save.
        nav.begin();
        nav.open(PLACES_SCOPE);
        nav.item_id(place);
        nav.close();
        nav.open(LIST_SCOPE);
        nav.item_id(row);
        nav.close();
        nav.open(FIELD_SCOPE);
        nav.item_id(field);
        nav.close();
        nav.open(BUTTON_SCOPE);
        nav.item_id(save);
        nav.close();
        nav.finish(&ctx);

        assert_eq!(nav.step(place, Dir::Down), Some(row), "shortcuts lead to the listing");
        assert_eq!(nav.step(row, Dir::Down), Some(field), "the listing leads to the name");
        assert_eq!(nav.step(field, Dir::Down), Some(save), "the name leads to the button");
        assert_eq!(nav.step(save, Dir::Down), None, "nothing below the footer");
        assert_eq!(nav.step(save, Dir::Up), Some(field));
        assert_eq!(nav.step(field, Dir::Up), Some(row), "and back to the listing");
        assert_eq!(nav.step(row, Dir::Up), Some(place));
    }

    /// An empty folder draws no rows at all - and a scope with no members used
    /// to be where a Down press died, leaving the name field and Save button
    /// unreachable from the keyboard. The scope's exit has to be followed
    /// through the empty listing; see `Nav::enter`.
    #[test]
    fn an_empty_folder_still_lets_the_arrows_reach_the_footer() {
        use crate::nav::{Dir, Nav};

        let (place, field, save) =
            (Id::new("place"), Id::new("field"), Id::new("save"));
        let mut nav = Nav::new(&scopes());
        let ctx = egui::Context::default();

        nav.begin();
        nav.open(PLACES_SCOPE);
        nav.item_id(place);
        nav.close();
        nav.open(LIST_SCOPE); // no rows in this folder
        nav.close();
        nav.open(FIELD_SCOPE);
        nav.item_id(field);
        nav.close();
        nav.open(BUTTON_SCOPE);
        nav.item_id(save);
        nav.close();
        nav.finish(&ctx);

        assert_eq!(nav.step(place, Dir::Down), Some(field), "Down reaches the name");
        assert_eq!(nav.step(field, Dir::Down), Some(save), "then Save");
        assert_eq!(nav.step(save, Dir::Up), Some(field));
        assert_eq!(nav.step(field, Dir::Up), Some(place), "and back to the shortcuts");
    }

    /// A press on the up button keeps its focus ring in the folder above: the
    /// button is still there, and a viewer often climbs more than one level.
    /// Only when it goes grey - at a root, where there is no further up - is
    /// the ring handed down to the listing. Every other way into a folder
    /// re-anchors as it did before.
    #[test]
    fn the_up_button_keeps_its_ring_until_it_goes_grey() {
        assert!(!focuses_listing_after_walk(true, true), "an enabled button keeps its ring");
        assert!(focuses_listing_after_walk(true, false), "a grey button hands it down");
        assert!(focuses_listing_after_walk(false, true), "a shortcut re-anchors");
        assert!(focuses_listing_after_walk(false, false), "a directory row re-anchors");

        let root = scratch("up-ring");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let mut picker = FilePicker::start(nested.clone(), Vec::new(), Mode::Open, String::new());
        picker.ask = Some("taken.json".to_owned());
        picker.walk(&root, true);
        assert_eq!(picker.listing.dir, root);
        assert!(!picker.reanchor, "the parent still has a parent of its own");
        assert!(picker.ask.is_none(), "the question belonged to the folder just left");

        picker.walk(&nested, false);
        assert_eq!(picker.listing.dir, nested);
        assert!(picker.reanchor, "walking into a folder is not the up button");
    }

    #[test]
    fn an_open_window_has_no_file_to_write() {
        let dir = scratch("open-target");
        let picker = FilePicker::start(dir, Vec::new(), Mode::Open, "xgvconfig.json".to_owned());
        assert!(!picker.saving());
    }

    /// A save onto a name that is already in the folder writes nothing until
    /// the question it puts up is answered, and a name that is free is written
    /// on the press.
    #[test]
    fn a_save_over_a_name_that_is_there_asks_before_it_writes() {
        let dir = scratch("asks-first");
        fs::write(dir.join("taken.json"), b"{}").unwrap();

        let mut picker = FilePicker::start(dir.clone(), Vec::new(), Mode::Save { sealed: false }, "taken.json".to_owned());
        let target = picker.target();
        assert_eq!(picker.press(target), SavePress::Asking, "the file is there");
        assert_eq!(picker.ask.as_deref(), Some("taken.json"), "the question is about that name");
        assert!(picker.ask_focus, "the question opens on the answer that changes nothing");

        // Asked already: a second press does not queue a second question.
        // Only the button writes, and it writes the path under the question.
        let target = picker.target();
        assert_eq!(picker.press(target), SavePress::Asking);

        let mut picker = FilePicker::start(dir, Vec::new(), Mode::Save { sealed: false }, "fresh.json".to_owned());
        let target = picker.target();
        assert!(matches!(picker.press(target), SavePress::Write(_)), "nothing to ask about");
        assert!(picker.ask.is_none());
    }

    /// The question is about the name the field held when it was asked. A
    /// viewer who types another name has answered it by meaning something else,
    /// and one who backs out of it gets the field back with the name in it.
    #[test]
    fn the_question_goes_when_the_name_does() {
        let dir = scratch("ask-drops");
        fs::write(dir.join("taken.json"), b"{}").unwrap();
        let mut picker = FilePicker::start(dir, Vec::new(), Mode::Save { sealed: false }, "taken.json".to_owned());
        let target = picker.target();
        assert_eq!(picker.press(target), SavePress::Asking);

        picker.name = "other.json".to_owned();
        picker.sync_ask();
        assert!(picker.ask.is_none(), "a question about a name that has gone");

        // Back on the question: taken down, and the field comes back to be
        // typed into rather than the window losing its ring.
        picker.name = "taken.json".to_owned();
        let target = picker.target();
        assert_eq!(picker.press(target), SavePress::Asking);
        assert!(picker.dismiss_ask());
        assert!(picker.ask.is_none() && picker.refocus_field);
        assert!(!picker.dismiss_ask(), "nothing left to take down");
    }
}
