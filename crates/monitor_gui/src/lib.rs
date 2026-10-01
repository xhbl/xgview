//! XGView GUI: egui + wgpu grid viewer.
//!
//! * [`app`]     – application state, navigation, render loop.
//! * [`grid`]    – grid geometry and tile painting.
//! * [`dialogs`] – device discovery and camera import dialogs.
//! * [`theme`]   – colour palette and typography.
//! * [`video`]   – uploads the decoded planes and turns them into pictures.
//!
//! The renderer is driven by the [`monitor_core::scheduler::Scheduler`], which
//! decides which channel is decoded with which stream, and by the
//! [`monitor_core::pipeline::ChannelManager`], which owns the streaming tasks.
//! Neither socket nor decoder ever runs on the UI thread.

pub mod app;
pub mod dialogs;
pub mod grid;
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
}

impl RunOptions {
    /// Effective configuration file path.
    pub fn config_path(&self) -> PathBuf {
        self.config_path.clone().unwrap_or_else(AppConfig::default_path)
    }
}

/// Builds the tokio runtime used by every background job.
fn build_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("xgview-io")
        .enable_all()
        .build()?)
}

fn native_options(fullscreen: bool) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(monitor_core::APP_DISPLAY_NAME)
            .with_inner_size([1280.0, 720.0])
            .with_min_inner_size([560.0, 360.0])
            .with_fullscreen(fullscreen),
        vsync: true,
        ..Default::default()
    }
}

/// Starts the viewer on the desktop targets (Windows / Linux / macOS).
pub fn run(options: RunOptions) -> anyhow::Result<()> {
    let runtime = build_runtime()?;
    let handle = runtime.handle().clone();
    let fullscreen = options.fullscreen || options.config.start_fullscreen;
    let config_path = options.config_path();

    eframe::run_native(
        monitor_core::APP_DISPLAY_NAME,
        native_options(fullscreen),
        Box::new(move |cc| {
            Ok(Box::new(XgViewApp::new(cc, options, config_path, handle)) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|err| anyhow::anyhow!("cannot start the viewer: {err}"))
}

/// Starts the viewer from the Android `android_main` entry point.
#[cfg(target_os = "android")]
pub fn run_android(app: android_activity::AndroidApp) -> anyhow::Result<()> {
    let runtime = build_runtime()?;
    let handle = runtime.handle().clone();

    // Application private storage is the only writable location guaranteed to
    // exist on every Android version.
    if let Some(dir) = app.internal_data_path() {
        std::env::set_var("XGVIEW_HOME", dir);
    }
    let config_path = AppConfig::default_path();
    let config = AppConfig::load_or_default(&config_path);
    let options = RunOptions {
        config,
        config_path: Some(config_path.clone()),
        fullscreen: true,
        from_autostart: true,
    };

    let mut native = native_options(true);
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
