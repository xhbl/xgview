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
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(monitor_core::APP_DISPLAY_NAME)
            .with_inner_size([1280.0, 720.0])
            .with_min_inner_size([560.0, 360.0]),
        vsync: true,
        ..Default::default()
    }
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

/// Sends a Rust panic to logcat.
///
/// A panic that leaves `android_main` cannot unwind - it is an `extern "C"`
/// entry point - so the process is aborted and the activity finishes with
/// nothing in the log but a `SIGABRT` whose backtrace ends inside the library.
/// The hook is what turns that into the message and the source location.
#[cfg(target_os = "android")]
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|panic| {
        let message = panic
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_string())
            .or_else(|| panic.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic without a message".to_string());
        let location = panic
            .location()
            .map(|at| format!("{}:{}:{}", at.file(), at.line(), at.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        tracing::error!(target: "xgview", %message, %location, "panicked");
    }));
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
        from_autostart: true,
        extra_lang_dir,
    };

    let mut native = native_options();
    native.android_app = Some(app);

    eframe::run_native(
        monitor_core::APP_DISPLAY_NAME,
        native,
        Box::new(move |cc| {
            Ok(Box::new(XgViewApp::new(cc, options, config_path, handle)) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|err| anyhow::anyhow!("cannot start the viewer: {err}"))
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
