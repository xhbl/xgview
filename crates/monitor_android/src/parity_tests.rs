//! Guards against the two front ends growing separate copies of the same
//! thing again.
//!
//! `MainActivity` (a `NativeActivity`) and `SurfaceActivity` (a plain
//! `Activity`) cannot share a base class, so their shared code was once copied
//! verbatim between them. The soft keyboard was ~100 lines duplicated, and the
//! language packs were unpacked once in Java and once in Rust. Both drifted:
//! the `SDK_INT < R` gate on the keyboard's height poll was added to one copy
//! and not the other (a keyboard that came up and was pulled straight back down
//! on a television box), and the two unpackers once wrote their `.ftl` files to
//! different directories (a clean install that stayed in English).
//!
//! These are structural tests read from the Java sources, so they run on the
//! desktop with `cargo test` and need no device. They do not check behaviour -
//! only that there is one implementation of each thing, that the few rules
//! inside it that history showed to be fragile are still there, and that the
//! bridge rules the native side relies on are still in place. If a future
//! change deliberately collapses more of this - one set of JNI entry points,
//! say - update the guard in the same commit; a failing test is the prompt to
//! decide that on purpose rather than by accident.

#[cfg(test)]
mod tests {
    use std::path::Path;

    const MAIN: &str = include_str!("../android/java/com/xhbl/xgview/MainActivity.java");
    const SURFACE: &str = include_str!("../android/java/com/xhbl/xgview/SurfaceActivity.java");
    const HOST: &str = include_str!("../android/java/com/xhbl/xgview/TextInputHost.java");

    /// Read at run time rather than `include_str!`: the file belongs to another
    /// crate in this workspace, so it is not part of this crate's package.
    fn gui_lib() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../monitor_gui/src/lib.rs");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
    }

    /// The build script's comment names the unpacker. A stale name there is
    /// how a reader is sent looking for code that no longer exists.
    fn gradle_build() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../android/app/build.gradle.kts");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
    }

    /// Anything that is the *implementation* of a soft keyboard, rather than a
    /// call into one. If any of these reappears in an activity, the copy that
    /// was removed has started to grow back.
    const IME_MARKS: &[&str] = &[
        "new BaseInputConnection",
        "imePoll",
        "getInputMethodWindowVisibleHeight",
        "showSoftInput(",
        "hideSoftInputFromWindow(",
        "private String composing",
    ];

    const JNI_DECLARATIONS: &[&str] = &[
        "nativeCommit(String text)",
        "nativeBackspace(int count)",
        "nativeEnter()",
        "nativeKeyboardHidden()",
    ];

    #[test]
    fn the_keyboard_has_one_implementation() {
        for mark in IME_MARKS {
            assert!(HOST.contains(mark), "TextInputHost lost {mark}");
            assert!(!MAIN.contains(mark), "MainActivity grew its own {mark}");
            assert!(!SURFACE.contains(mark), "SurfaceActivity grew its own {mark}");
        }
    }

    #[test]
    fn both_front_ends_hold_the_host_and_only_wire_it_up() {
        for activity in [MAIN, SURFACE] {
            for glue in [
                "private TextInputHost textInput;",
                "textInput.setWanted(wanted, multiline);",
                "textInput.install(",
                "textInput.onImeVisibility(visible);",
                "nativeCommit(text);",
                "nativeBackspace(count);",
                "nativeEnter();",
                "nativeKeyboardHidden();",
            ] {
                assert!(activity.contains(glue), "an activity lost its glue: {glue}");
            }
            for declaration in JNI_DECLARATIONS {
                assert!(
                    activity.contains(declaration),
                    "an activity lost a JNI declaration: {declaration}"
                );
            }
        }
    }

    #[test]
    fn the_host_keeps_the_rules_that_once_drifted() {
        // The poll is only started below R; the Runnable itself also refuses
        // to run on R and above. The second guard was in one old copy and not
        // the other. Both live in one place now.
        assert!(HOST.contains("Build.VERSION.SDK_INT < Build.VERSION_CODES.R"));
        assert!(HOST.contains("Build.VERSION.SDK_INT >= Build.VERSION_CODES.R"));
        assert!(!MAIN.contains("Build.VERSION.SDK_INT < Build.VERSION_CODES.R"));
        assert!(!SURFACE.contains("Build.VERSION.SDK_INT < Build.VERSION_CODES.R"));

        // Composing already put the text in the field. Committing the same
        // string must not backspace and retype it - that loses the cursor
        // position and miscounts surrogate pairs. Check the whole short
        // circuit rather than its condition alone: an early return that was
        // dropped while the condition stayed would pass a string-only guard.
        let on_commit = &HOST[HOST
            .find("private void onCommit(")
            .expect("TextInputHost lost onCommit")..];
        let body = &on_commit[..on_commit.find("\n    }").expect("onCommit has no end")];
        let equal = body.find("text.contentEquals(previous)").unwrap();
        let early_return = body.find("return;").unwrap();
        let backspace = body.find("nativeSide.backspace(previous.length());").unwrap();
        let commit = body.find("nativeSide.commit(text);").unwrap();
        assert!(
            equal < early_return && early_return < backspace && backspace < commit,
            "onCommit's composing short circuit was broken"
        );
        assert!(HOST.contains("if (!text.isEmpty())"));
    }

    #[test]
    fn the_static_bridge_still_points_one_way() {
        // Rust calls MainActivity's static methods. When the surfaces front end
        // owns the wall, MainActivity forwards; the other one must not forward
        // back or the two would call each other forever.
        assert!(MAIN.contains("SurfaceActivity.setKeyboardWanted(wanted, multiline);"));
        assert!(!SURFACE.contains("MainActivity.setKeyboardWanted"));
    }

    #[test]
    fn language_packs_are_unpacked_once_before_the_native_thread_starts() {
        assert_eq!(
            SURFACE.matches("static void copyLanguagePacks(").count(),
            1,
            "there should be exactly one unpacker"
        );
        assert_eq!(
            MAIN.matches("SurfaceActivity.copyLanguagePacks(this);").count(),
            1,
            "the old front end must use the same unpacker"
        );

        // A NativeActivity starts its native thread inside super.onCreate,
        // and XgViewApp::new scans the language directory there. A copy made
        // any later races that scan and a clean install stays in English.
        let copy = MAIN.find("SurfaceActivity.copyLanguagePacks(this);").unwrap();
        let super_on_create = MAIN.find("super.onCreate(savedInstanceState);").unwrap();
        assert!(
            copy < super_on_create,
            "the packs must be copied before super.onCreate starts the native thread"
        );
        let surface_copy = SURFACE.find("copyLanguagePacks(this);").unwrap();
        // The call, not the declaration of the same name further down the
        // file: finding the first bare `nativeStartWall(` could land there.
        let surface_start = SURFACE.find("wallToken = nativeStartWall(").unwrap();
        assert!(
            surface_copy < surface_start,
            "the packs must be copied before the wall's surface starts the loop"
        );

        // The `xgview` segment is the one that was once dropped, putting the
        // files where the catalogue does not look.
        assert!(SURFACE.contains("\"xgview/langs\""));
        assert!(!SURFACE.contains("new File(host.getFilesDir(), \"langs\")"));
        assert!(!MAIN.contains("assets.list("), "a second unpacker in MainActivity");
        // One bad asset must not stop the packs listed after it. The old Rust
        // unpacker handled each one on its own, and the NativeActivity front
        // end used to run that one.
        assert!(
            SURFACE.contains(r#""cannot copy language pack " + name"#),
            "a broken pack must not stop the ones after it"
        );

        let gui = gui_lib();
        assert!(!gui.contains("extract_lang_assets"), "the Rust unpacker came back");
        assert!(!gui.contains("open_dir(&root)"), "the Rust asset walk came back");

        let gradle = gradle_build();
        assert!(
            !gradle.contains("extract_lang_assets"),
            "the build script still points at the Rust unpacker"
        );
        assert!(
            gradle.contains("SurfaceActivity.copyLanguagePacks"),
            "the build script should name the Java unpacker"
        );
    }

    /// The activity's window is alive on a grid too, while its view is moved
    /// off screen. A tile pinned to the main stream is just as wide as a
    /// magnified picture, and a codec that asks only for a window renders into
    /// that hidden view: the grid waits for a first frame for ever. Three ends
    /// hold the rule together - the owner sets the want, the want gates the
    /// layer, and a session left on the surface reopens on byte buffers.
    #[test]
    fn a_grid_tile_keeps_its_frames_off_the_hidden_surface() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let read = |name: &str| {
            let path = root.join(name);
            std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
        };
        let surface = read("monitor_codec/src/surface.rs");
        let codec = read("monitor_codec/src/amediacodec.rs");
        let gui = read("monitor_gui/src/android.rs");

        assert!(
            surface.contains("!suspended && wanted && window"),
            "a window without a magnified view must not be an active layer"
        );
        assert!(
            gui.contains("set_surface_layer_wanted(owner.is_some())"),
            "only a magnified view may ask for the surface"
        );
        assert!(
            codec.contains("if video_layer_active() && self.surface_eligible"),
            "the decoder must ask for the layer, not just for a window"
        );
        assert!(
            codec.contains("if self.on_surface && !wanted_now"),
            "a session left on the surface must reopen for the grid"
        );
        assert!(
            codec.contains("the magnified view asked for the surface; reopening the decoder"),
            "magnifying a pinned tile must switch to the surface on purpose"
        );
        assert!(
            codec.contains("self.surface_attempted = true;")
                && codec.contains("&& !self.surface_attempted"),
            "the reverse switch must be a one-off, and must record the attempt"
        );
        assert!(
            codec.contains("self.surface_attempted = false;") && codec.contains("if !wanted_now {"),
            "a device that falls back to byte buffers must not be retried every frame, but the next magnification gets a fresh attempt"
        );
        assert!(
            codec.contains("&& video_layer_active()"),
            "the reverse switch must wait until the surface can really be had"
        );
    }

    /// Android's export writes into the public `Download/xgview` folder, and
    /// the import window opens on that same folder. The two halves are spelled
    /// out in different files - the activity writes the file, the window walks
    /// to it - so a rename in one place has to reach the other, and the export
    /// must keep going out through the activity rather than through `std::fs`
    /// into a folder no file manager can see.
    #[test]
    fn an_export_lands_where_the_import_looks_for_it() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let gui = std::fs::read_to_string(root.join("monitor_gui/src/app/persist.rs"))
            .expect("cannot read persist.rs");
        assert!(
            MAIN.contains("EXPORT_FOLDER = \"Download/xgview\""),
            "the activity writes into Download/xgview"
        );
        assert!(
            gui.contains("pub const EXPORT_SUBFOLDER: &str = \"xgview\""),
            "the picker must name the same subfolder the activity writes into"
        );
        assert!(
            gui.contains("crate::android::export_folder().filter(|dir| readable(dir))"),
            "the import window must open on the export folder when it can be walked"
        );
        assert!(
            gui.contains("FileJob::Export(passphrase) => self.export_to_download(passphrase)"),
            "Android's export must keep writing into the public folder"
        );
        assert!(
            gui.contains("crate::android::request_storage()"),
            "the public folder needs the Android 9 permission asked for before it is offered"
        );
    }

    /// Neither half of the configuration pair may go ahead before the storage
    /// permission it asked for is in hand. Asking is asynchronous - the answer
    /// is a dialog in another window - so an import window opened first listed
    /// the app's own folder and nothing of the volume it was opened for, and an
    /// export attempted first was refused by the platform and told the viewer to
    /// press a button that had already been pressed.
    #[test]
    fn both_halves_wait_for_the_storage_answer() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let gui = std::fs::read_to_string(root.join("monitor_gui/src/app/persist.rs"))
            .expect("cannot read persist.rs");
        let android = std::fs::read_to_string(root.join("monitor_gui/src/android.rs"))
            .expect("cannot read android.rs");
        assert!(
            MAIN.contains("public static boolean hasStoragePermission()"),
            "the activity must answer whether the permission is held"
        );
        assert!(
            android.contains("call_bool(\"hasStoragePermission\")"),
            "the native side must ask the activity rather than guess"
        );
        assert!(
            gui.matches("if !crate::android::storage_granted() && crate::android::request_storage()")
                .count()
                >= 2,
            "an import and an export must each ask before they touch the public folder"
        );
        assert!(
            gui.contains("fn wait_for_storage")
                && gui.contains("StorageJob::Import, \"toast-import-grant-storage\"")
                && gui.contains("StorageJob::Export(passphrase), \"toast-export-grant-storage\""),
            "a waiting press has to be remembered with what it was going to do"
        );
        assert!(
            gui.contains("fn poll_storage_pending")
                && gui.contains("StorageJob::Import | StorageJob::ImportAllFiles => self.open_import_picker()")
                && gui.contains("StorageJob::Export(passphrase) => self.write_export_now(passphrase)"),
            "the press that asked has to be carried out on the frame the answer arrives"
        );
        assert!(
            gui.matches("self.open_import_picker()").count() >= 3,
            "the window must open from the poll as well as from the press"
        );
    }

    /// From Android 11 the shared volume is filtered by owner, so a
    /// configuration copied on from another machine is not in the listing at
    /// all. The grant that lifts it is a switch in the system settings, which
    /// answers nothing: the wait for it can only see the viewer come back, and
    /// has to end there - granted or not - instead of leaving the press held.
    #[test]
    fn a_device_that_cannot_draw_is_told_so_and_offered_a_way_out() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let surface = std::fs::read_to_string(root.join("monitor_gui/src/android_surface.rs"))
            .expect("cannot read android_surface.rs");
        let android = std::fs::read_to_string(root.join("monitor_gui/src/android.rs"))
            .expect("cannot read android.rs");
        assert!(
            surface.contains("SURFACE_FAILURES_BEFORE_NOTICE")
                && surface.contains("notice_shown = true")
                && surface.contains("crate::android::show_unsupported_device("),
            "a surface that can never be made must end in the notice, not an endless retry"
        );
        assert!(
            surface.contains("monitor_i18n::tr(\"unsupported-title\")")
                && surface.contains("monitor_i18n::tr(\"unsupported-body\")")
                && surface.contains("monitor_i18n::tr(\"unsupported-exit\")"),
            "the notice is the viewer's text and belongs in the language packs"
        );
        assert!(
            android.contains("showUnsupported")
                && android.contains("(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V"),
            "the native side must call the activity's notice with three strings"
        );
        assert!(
            MAIN.contains("public static void showUnsupported(")
                && MAIN.contains("setCancelable(false)")
                && MAIN.contains("Process.killProcess"),
            "the notice has to be modal and end the viewer when acknowledged"
        );
        assert!(
            MAIN.contains("reqGlEsVersion"),
            "the notice names the OpenGL ES level the device actually offers"
        );
    }

    #[test]
    fn an_import_offers_the_all_files_switch_and_waits_for_the_trip_back() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let gui = std::fs::read_to_string(root.join("monitor_gui/src/app/persist.rs"))
            .expect("cannot read persist.rs");
        let android = std::fs::read_to_string(root.join("monitor_gui/src/android.rs"))
            .expect("cannot read android.rs");
        let manifest = std::fs::read_to_string(root.join("monitor_android/android/AndroidManifest.xml"))
            .expect("cannot read the manifest");
        assert!(
            manifest.contains("android.permission.MANAGE_EXTERNAL_STORAGE"),
            "the manifest must declare the grant the switch turns on"
        );
        assert!(
            MAIN.contains("public static boolean hasAllFilesAccess()")
                && MAIN.contains("Environment.isExternalStorageManager()"),
            "the activity must answer whether the whole volume is readable"
        );
        assert!(
            MAIN.contains("ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION"),
            "the press has to reach the screen the switch lives on"
        );
        assert!(
            android.contains("call_bool(\"hasAllFilesAccess\")")
                && android.contains("call_bool(\"requestAllFilesAccess\")"),
            "the native side must ask the activity for both"
        );
        assert!(
            gui.contains("!crate::android::all_files_access() && !self.all_files_asked")
                && gui.contains("StorageJob::ImportAllFiles"),
            "an import without it must offer the switch, once per run"
        );
        assert!(
            gui.contains("crate::android::resume_count() != pending.resumes"),
            "the wait must end when the viewer comes back, since the switch reports nothing"
        );
        assert!(
            gui.contains("toast-import-all-files-missing"),
            "a wait that ends without the grant has to say what is missing"
        );
    }
}
