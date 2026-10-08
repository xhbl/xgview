//! XGView GUI: egui + wgpu grid viewer.
//!
//! * [`app`]     – application state, navigation, render loop.
//! * [`controls`] – value controls that adjust with the sideways keys.
//! * [`dialogs`] – device discovery and camera import dialogs.
//! * [`fonts`]   – system font fallback for the characters egui does not ship.
//! * [`grid`]    – grid geometry and tile painting.
//! * [`icons`]   – the shapes the controls carry.
//! * [`keyboard`] – the soft keyboard on Android (Android only).
//! * [`nav`]     – explicit directional navigation for a remote control.
//! * [`theme`]   – colour palette and typography.
//! * [`video`]   – uploads the decoded planes and turns them into pictures.
//!
//! The renderer is driven by the [`monitor_core::scheduler::Scheduler`], which
//! decides which channel is decoded with which stream, and by the
//! [`monitor_core::pipeline::ChannelManager`], which owns the streaming tasks.
//! Neither socket nor decoder ever runs on the UI thread.

pub mod app;
pub mod blackout;
pub mod controls;
pub mod dialogs;
pub mod fonts;
pub mod grid;
pub mod icons;
#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "android")]
pub mod keyboard;
pub mod nav;
pub mod theme;
pub mod video;

pub use app::XgViewApp;

use std::path::PathBuf;

use monitor_core::config::AppConfig;

/// Options handed over by the `xgview` binary.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Configuration already loaded by the entry point.
    pub config: AppConfig,
    /// Path the configuration is read from / written to.
    pub config_path: Option<PathBuf>,
    /// Start in full screen (TV / kiosk deployment).
    pub fullscreen: bool,
    /// The process was started by the start-on-boot registration.
    pub from_autostart: bool,
    /// A directory language packs are looked for in, beside the one in the
    /// configuration directory. Android names its own external data directory
    /// there, which a file transfer can reach; the desktop has none of its own
    /// and passes `None`.
    pub extra_lang_dir: Option<PathBuf>,
}

impl RunOptions {
    /// Effective configuration file path.
    pub fn config_path(&self) -> PathBuf {
        self.config_path.clone().unwrap_or_else(AppConfig::default_path)
    }
}

/// Worker threads of the streaming runtime.
///
/// Every channel reads its socket and decodes on whichever worker it is
/// scheduled to, and decoding is a blocking call: while a worker is inside
/// `decode` it cannot read any other channel's socket. Two workers were enough
/// to keep up on average and still left a socket unread long enough for the
/// kernel to drop the tail of a burst, which is what a high bitrate UDP stream
/// arrives as. The pool therefore follows the CPU count, bounded so that a
/// machine with many cores does not spawn threads the decoding load cannot use.
fn worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(2)
        .clamp(2, 8)
}

/// Builds the tokio runtime used by every background job.
fn build_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads())
        .thread_name("xgview-io")
        .enable_all()
        .build()?)
}

/// Window options for the targets that run eframe: the desktop and Android.
///
/// Full screen is deliberately not asked for on the `ViewportBuilder`: it is
/// applied before the window knows which monitor it is on, and on Windows that
/// leaves the window covering a window-sized area rather than the screen - the
/// same wrong geometry that toggling F11 twice repairs by hand. The app sends
/// the command from its first frame instead, once the monitor is known; see the
/// `fullscreen_pending` field on `XgViewApp`.
fn native_options() -> eframe::NativeOptions {
    let viewport = egui::ViewportBuilder::default()
        .with_title(monitor_core::APP_DISPLAY_NAME)
        .with_inner_size([1280.0, 720.0])
        .with_min_inner_size([560.0, 360.0]);

    // The desktop window carries the same artwork the executable and the app
    // bundle do. Android takes its launcher icon from the APK resources
    // instead, and a `ViewportBuilder` icon means nothing to it.
    #[cfg(not(target_os = "android"))]
    let viewport = viewport.with_icon(std::sync::Arc::new(window_icon()));

    eframe::NativeOptions {
        viewport,
        vsync: true,
        ..Default::default()
    }
}

/// The window / taskbar icon of the desktop builds, decoded at startup from the
/// PNG the Linux package installs by the same name (see `assets/icons/`). The
/// Windows executable carries the `.ico` over this, so the two stay the same
/// picture by construction rather than by being kept in sync by hand.
#[cfg(not(target_os = "android"))]
fn window_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!(
        "../../../assets/icons/linux/hicolor/256x256/apps/xgview.png"
    ))
    .expect("window icon: assets/icons/linux/hicolor/256x256/apps/xgview.png is not a valid PNG")
}

/// Starts the viewer on the desktop targets (Windows / Linux / macOS).
pub fn run(options: RunOptions) -> anyhow::Result<()> {
    let runtime = build_runtime()?;
    let handle = runtime.handle().clone();
    let config_path = options.config_path();

    eframe::run_native(
        monitor_core::APP_DISPLAY_NAME,
        native_options(),
        Box::new(move |cc| {
            Ok(Box::new(XgViewApp::new(cc, options, config_path, handle)) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|err| anyhow::anyhow!("cannot start the viewer: {err}"))
}

#[cfg(target_os = "android")]
#[link(name = "log")]
extern "C" {
    fn __android_log_print(
        priority: std::ffi::c_int,
        tag: *const std::ffi::c_char,
        format: *const std::ffi::c_char,
        ...
    ) -> std::ffi::c_int;
}

/// Sends the viewer's own `tracing` output to logcat.
///
/// Rust's logging goes to stdout, which a `NativeActivity` throws away, so
/// without this every line the viewer emits on Android - the codec diagnostics,
/// the session windows, the errors - is lost. `adb logcat -s xgview` reads it
/// instead, and nothing else is needed to debug a start that goes wrong.
#[cfg(target_os = "android")]
fn init_android_logging() {
    use std::ffi::CString;
    use std::io::Write;

    // `ANDROID_LOG_DEBUG`
    const ANDROID_LOG_DEBUG: std::ffi::c_int = 3;

    struct Logcat;

    impl Write for Logcat {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            for line in buf.split(|byte| *byte == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let Ok(text) = CString::new(line) else {
                    continue;
                };
                // Safety: a NUL terminated format string with no conversions,
                // fed one `char *` for its single `%s`.
                unsafe {
                    __android_log_print(
                        ANDROID_LOG_DEBUG,
                        c"xgview".as_ptr(),
                        c"%s".as_ptr(),
                        text.as_ptr(),
                    );
                }
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logcat {
        type Writer = Logcat;

        fn make_writer(&'a self) -> Self::Writer {
            Logcat
        }
    }

    // An already installed subscriber is left alone: failing to install only
    // means the process was started twice, which is not worth an error.
    let _ = tracing_subscriber::fmt()
        .with_writer(Logcat)
        .with_max_level(tracing::Level::DEBUG)
        .with_target(true)
        .try_init();
}

/// The message a panic payload carries, or a stand-in when it carries none.
#[cfg(target_os = "android")]
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic without a message".to_string())
}

/// Sends a Rust panic to logcat.
///
/// A panic that leaves `android_main` cannot unwind - it is an `extern "C"`
/// entry point, and Rust aborts at such a boundary rather than unwinding out of
/// it - so the process dies with a `SIGABRT` and nothing in the log but a
/// backtrace ending inside the library. The hook is what turns that into the
/// message and the source location.
///
/// The hook itself neither aborts nor exits: it logs and lets the panic carry
/// on unwinding, which is what leaves the `catch_unwind` in [`run_android`] a
/// chance to intercept it.
#[cfg(target_os = "android")]
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|panic| {
        let message = panic_message(panic.payload());
        let location = panic
            .location()
            .map(|at| format!("{}:{}:{}", at.file(), at.line(), at.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        tracing::error!(target: "xgview", %message, %location, "panicked");
    }));
}

/// Whether the renderer - and with it the pipeline whose creation the old
/// Vulkan drivers fail - has come up in this process.
///
/// Set at the top of the app-creator closure, which eframe calls after
/// `Painter::set_window` has built that pipeline and before the first frame. It
/// is what separates a launch that could not draw - where the GL fallback is the
/// answer - from a panic hours into a run, where the backend has plainly worked
/// and the markers must be left alone. Without it, one panic on the main thread
/// at any time would write the attempt marker and leave a healthy device on GL
/// for good.
#[cfg(target_os = "android")]
static RENDERER_CAME_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Leaves the process at once, without unwinding, flushing or `atexit`.
///
/// `std::process::exit` is the wrong tool on the way out of a process whose
/// graphics driver has already failed: it runs the libc exit path, and a thread
/// still inside a driver call can block there or crash a second time. `_exit` is
/// the syscall itself.
#[cfg(target_os = "android")]
fn exit_now() -> ! {
    extern "C" {
        fn _exit(status: std::ffi::c_int) -> !;
    }
    // Safety: `_exit` takes a plain int and does not return.
    unsafe { _exit(0) }
}

/// Starts the viewer from the Android `android_main` entry point.
#[cfg(target_os = "android")]
pub fn run_android(app: android_activity::AndroidApp) -> anyhow::Result<()> {
    init_android_logging();
    install_panic_hook();
    let runtime = build_runtime()?;
    let handle = runtime.handle().clone();

    // Application private storage is the only writable location guaranteed to
    // exist on every Android version.
    if let Some(dir) = app.internal_data_path() {
        std::env::set_var("XGVIEW_HOME", dir);
    }
    // The language packs shipped as APK assets are extracted to the
    // configuration directory before `XgViewApp::new` runs, so the catalogue
    // built there discovers them alongside the embedded English fallback. A
    // pack dropped into the app's external data directory is found too, which
    // is the one place on Android a file transfer can put one.
    extract_lang_assets(&app);
    let extra_lang_dir = app.external_data_path().map(|dir| dir.join("langs"));
    let config_path = AppConfig::default_path();
    let config = AppConfig::load_or_default(&config_path);
    let options = RunOptions {
        config,
        config_path: Some(config_path.clone()),
        fullscreen: true,
        // Placeholder: on Android the launch intent - not this flag - says
        // whether the boot receiver started us, so `App::new` asks the activity.
        from_autostart: false,
        extra_lang_dir,
    };

    // Pick the backend before the event loop - and with it the renderer - is
    // built, because neither can be replaced afterwards.
    //
    // Two signals send a launch to the GL backend, and one overrides both:
    //
    // * the Vulkan gate. `MainActivity` writes its marker before the native
    //   thread is even started when the device's Vulkan driver is older than
    //   1.1 - the family whose drivers lose the device while the first pipeline
    //   is built - so it is already in place here; see [`vulkan_gate_marker`].
    // * the attempt marker, written just below before Vulkan is tried, and left
    //   behind by a launch that panicked or aborted before its first frame; see
    //   [`vulkan_attempt_marker`].
    //
    // A `force-vulkan` file overrides the gate, so the fallback can still be
    // exercised on the very device the gate keeps off Vulkan; see
    // [`force_vulkan_requested`]. It deliberately does *not* override an
    // attempt that already failed: the launch it forces onto Vulkan is the one
    // that may not come back, so the launch after it has to fall back rather
    // than force its way into the same abort - and the restart the caught panic
    // asks for is itself a launch like that.
    let marker = vulkan_attempt_marker();
    let previous_attempt_aborted = marker.as_deref().is_some_and(|path| path.exists());
    let gated_off_vulkan = vulkan_gate_marker()
        .as_deref()
        .is_some_and(|path| path.exists());
    let forced = force_vulkan_requested(&app);
    let use_gl = previous_attempt_aborted || (gated_off_vulkan && !forced);

    // The gate is a reason of its own, and one that does not go away, so an
    // attempt marker sitting under it says nothing true: the launch before this
    // one did not abort because Vulkan failed, it never tried Vulkan. Dropping
    // the marker keeps the log honest and leaves a forced run repeatable - the
    // next launch without `force-vulkan` clears it and returns to the gate.
    if gated_off_vulkan && !forced && previous_attempt_aborted {
        if let Some(path) = &marker {
            let _ = std::fs::remove_file(path);
            tracing::info!(
                target: "xgview",
                "the version gate supersedes the previous attempt; its marker is cleared"
            );
        }
    }

    if forced && gated_off_vulkan && !previous_attempt_aborted {
        tracing::warn!(
            target: "xgview",
            "force-vulkan is present; trying Vulkan although this device has no Vulkan 1.1"
        );
    }
    if use_gl {
        if gated_off_vulkan {
            tracing::warn!(
                target: "xgview",
                "this device has no Vulkan 1.1; using the GL backend"
            );
        } else {
            tracing::warn!(
                target: "xgview",
                "the previous launch did not reach its first frame; using the GL backend"
            );
        }
        std::env::set_var("WGPU_BACKEND", "gl");
    } else if let Some(path) = &marker {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, b"");
        tracing::info!(
            target: "xgview",
            "marked this launch as a Vulkan attempt; the mark is cleared once the renderer is up"
        );
    }

    let mut native = native_options();
    native.android_app = Some(app);

    // The renderer is built inside this call, and on the drivers this whole
    // dance exists for the device is lost while the first pipeline is made:
    // wgpu's uncaptured error handler panics. Left alone the panic runs to the
    // `extern "C" android_main` entry point, which cannot unwind - Rust aborts
    // there and the process dies with a `SIGABRT` whose only readable trace is
    // the hook's line. Catching it here keeps the payload, records the marker
    // and hands the launch to a fresh process, which comes up on the GL
    // backend; see [`restart_soon`].
    //
    // This is the fallback, not the main path: the gate above is meant to keep
    // a known-old driver off Vulkan in the first place.
    let marker_for_run = marker.clone();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        eframe::run_native(
            monitor_core::APP_DISPLAY_NAME,
            native,
            Box::new(move |cc| {
                // eframe calls this after `Painter::set_window` has built the
                // render pipeline - the very `create_render_pipeline` the
                // drivers above fail - and before the first frame, so from here
                // on a panic is a run-time one rather than a launch that could
                // not draw; see the `match` below.
                RENDERER_CAME_UP.store(true, std::sync::atomic::Ordering::Relaxed);
                // Reaching this point also means an attempt that got here did
                // not abort and may clear its marker. The GL fallback keeps it:
                // that is what stops the next launch from trying Vulkan all
                // over again.
                if !use_gl {
                    if let Some(path) = &marker_for_run {
                        let _ = std::fs::remove_file(path);
                        tracing::info!(
                            target: "xgview",
                            "the renderer came up on Vulkan; its attempt marker is cleared"
                        );
                    }
                }
                Ok(Box::new(XgViewApp::new(cc, options, config_path, handle))
                    as Box<dyn eframe::App>)
            }),
        )
    }));

    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(anyhow::anyhow!("cannot start the viewer: {err}")),
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            // The catch covers the whole life of the process, so which panic
            // this is decides what is left to do. Only the last case is a
            // launch that could not draw on the backend it was handed.
            if RENDERER_CAME_UP.load(std::sync::atomic::Ordering::Relaxed) {
                // The wall was up, so the backend is not what broke. Restart
                // the process and leave the markers alone: writing them here is
                // exactly what would pin a healthy device to GL for good.
                tracing::error!(
                    target: "xgview",
                    %message,
                    "the renderer panicked while running; restarting the viewer"
                );
            } else if use_gl {
                // GL is the fallback, so there is nothing left to come back on
                // and a restart would only replay it - a device whose driver can
                // do neither backend (§12's PowerVR, whose GLES 3.1 has no
                // `GL_KHR_debug`) would loop forever. Leave it down and report
                // the panic as the error it is.
                tracing::error!(
                    target: "xgview",
                    %message,
                    "the renderer panicked before it came up, on the GL backend; there is nothing left to fall back to"
                );
                return Err(anyhow::anyhow!("the renderer panicked: {message}"));
            } else {
                tracing::error!(
                    target: "xgview",
                    %message,
                    "the renderer panicked before it came up; the next launch uses the GL backend"
                );
                // Belt to the marker's braces: it was written before Vulkan was
                // tried, but a launch forced past that marker still has to leave
                // one behind.
                if let Some(path) = &marker {
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(path, b"");
                }
            }
            // The two cases that have a backend to come back on ask for a fresh
            // process. The activity declines when its own accounting says the
            // app is in a crash loop, in which case this one is left down too.
            if android::restart_soon() {
                exit_now();
            }
            Err(anyhow::anyhow!("the renderer panicked: {message}"))
        }
    }
}

/// The file whose presence says the Vulkan backend aborted a launch before it
/// could draw, so the next one should use the GL backend instead.
///
/// Some old Android Vulkan drivers - Adreno on Android 8.x, Vulkan 1.0 -
/// enumerate an adapter and hand out a device, then lose that device while the
/// first pipeline is built, which aborts the process before anything is drawn.
/// Nothing can be asked about that in advance, so the attempt is the test: the
/// marker is written before Vulkan is used and survives such an abort, and the
/// next launch reads it and falls back to GL, which those drivers serve. A
/// machine with a working Vulkan never notices it, because the launch that
/// draws clears it again.
///
/// It sits beside the configuration and is only ever a marker; its contents are
/// not read.
#[cfg(target_os = "android")]
fn vulkan_attempt_marker() -> Option<PathBuf> {
    AppConfig::default_path().parent().map(|dir| dir.join("wgpu-no-vulkan"))
}

/// The file whose presence says the device's Vulkan driver is older than 1.1,
/// so Vulkan should not be tried at all.
///
/// The attempt marker below is the only way to find *some* old drivers out, and
/// it costs a launch: the first one aborts. The version, unlike the behaviour,
/// can be asked about in advance - `PackageManager` reports what the device
/// declares - so the launch that would abort is skipped altogether. That is a
/// Java question, and `MainActivity.onCreate` answers it before the native
/// thread is started, writing this marker when the answer is "no Vulkan 1.1"
/// and deleting a stale one when it is not. See `MainActivity.applyVulkanGate`.
///
/// It sits beside the configuration and is only ever a marker; its contents are
/// not read.
#[cfg(target_os = "android")]
fn vulkan_gate_marker() -> Option<PathBuf> {
    AppConfig::default_path()
        .parent()
        .map(|dir| dir.join("wgpu-vulkan-unsupported"))
}

/// Whether Vulkan is to be tried even where the *version gate* says not to.
///
/// The switch is a `force-vulkan` file in the application's external data
/// directory - the one place on Android a file transfer can put one - so the
/// `catch_unwind` fallback can still be exercised on the very device the gate
/// keeps off Vulkan (`adb push` an empty file there, or `adb shell touch
/// /sdcard/Android/data/com.xhbl.xgview/files/force-vulkan`). It exists for
/// nothing else, and a launch that finds it says so in the log.
///
/// The attempt marker is not overridden, only the gate: see the note on
/// `use_gl` in [`run_android`]. Deleting the attempt marker alongside this file
/// is what makes the fallback run again.
#[cfg(target_os = "android")]
fn force_vulkan_requested(app: &android_activity::AndroidApp) -> bool {
    app.external_data_path()
        .map(|dir| dir.join("force-vulkan").exists())
        .unwrap_or(false)
}

/// Copies the language packs shipped as APK assets to the configuration
/// directory, where `monitor_i18n` discovers them.
///
/// English is embedded in the binary, so only the extra packs (`zh-CN.ftl`,
/// …) are shipped as assets and extracted here. The files are small, so they
/// are overwritten every launch to pick up pack updates without a manual clear.
#[cfg(target_os = "android")]
fn extract_lang_assets(app: &android_activity::AndroidApp) {
    use std::ffi::CString;

    let manager = app.asset_manager();
    let root = CString::new("").expect("empty C string");
    let Some(dir) = manager.open_dir(&root) else {
        tracing::warn!(target: "xgview", "cannot open APK assets for language packs");
        return;
    };
    let dest = monitor_core::config::config_dir().join("langs");
    if let Err(err) = std::fs::create_dir_all(&dest) {
        tracing::warn!(target: "xgview", %err, path = %dest.display(), "cannot create language pack directory");
        return;
    }
    for name in dir {
        if !name.to_bytes().ends_with(b".ftl") {
            continue;
        }
        let Some(mut asset) = manager.open(&name) else {
            tracing::warn!(target: "xgview", name = %name.to_string_lossy(), "cannot open language asset");
            continue;
        };
        match asset.buffer() {
            Ok(bytes) => {
                let path = dest.join(name.to_string_lossy().as_ref());
                if let Err(err) = std::fs::write(&path, bytes) {
                    tracing::warn!(target: "xgview", %err, path = %path.display(), "cannot write language pack");
                }
            }
            Err(err) => tracing::warn!(target: "xgview", %err, name = %name.to_string_lossy(), "cannot read language asset"),
        }
    }
}
