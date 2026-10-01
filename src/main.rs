//! XGView entry point.
//!
//! A plain `cargo run` starts the viewer with the configuration found in the
//! platform configuration directory. Deployment helpers are exposed as
//! command line switches so that the Windows / Android install scripts can
//! register the start-on-boot entry without any UI interaction.

use std::path::{Path, PathBuf};

use anyhow::Result;
use monitor_core::autostart;
use monitor_core::config::AppConfig;
use monitor_core::layout::GridLayout;
use monitor_core::scheduler::Scheduler;
use monitor_core::{APP_AUTHOR, APP_DISPLAY_NAME};
use monitor_gui::RunOptions;

const HELP: &str = "\
XGView - a grid viewer for surveillance cameras

Usage: xgview [OPTIONS]

Options:
  --config <PATH>      Use an explicit configuration file
  --fullscreen         Start in full screen (TV / kiosk deployment)
  --windowed           Start in a window even if the configuration asks for full screen
  --autostart          Marker used by the start-on-boot registration
  --install-autostart  Register XGView for start-on-boot and exit
  --remove-autostart   Remove the start-on-boot registration and exit
  --print-schedule     Print the resolved grid / channel schedule and exit
  -h, --help           Print this help
  -V, --version        Print the version
";

fn main() -> Result<()> {
    init_logging();

    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!("{HELP}");
        return Ok(());
    }
    if args.iter().any(|arg| arg == "-V" || arg == "--version") {
        println!("{APP_DISPLAY_NAME} {} by {APP_AUTHOR}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if args.iter().any(|arg| arg == "--install-autostart") {
        autostart::set_enabled(true)?;
        println!("start-on-boot registered ({})", autostart::mechanism());
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--remove-autostart") {
        autostart::set_enabled(false)?;
        println!("start-on-boot registration removed");
        return Ok(());
    }

    let config_path = flag_value(&args, "--config").map(PathBuf::from);
    let config_path = config_path.unwrap_or_else(AppConfig::default_path);
    let config = AppConfig::load_or_default(&config_path);

    if args.iter().any(|arg| arg == "--print-schedule") {
        print_schedule(&config, &config_path);
        return Ok(());
    }

    let fullscreen = if args.iter().any(|arg| arg == "--windowed") {
        false
    } else {
        args.iter().any(|arg| arg == "--fullscreen") || config.start_fullscreen
    };

    tracing::info!(
        target: "xgview",
        path = %config_path.display(),
        cameras = config.cameras.len(),
        layout = config.layout.label(),
        "starting {APP_DISPLAY_NAME} {}",
        env!("CARGO_PKG_VERSION")
    );

    monitor_gui::run(RunOptions {
        config,
        config_path: Some(config_path),
        fullscreen,
        // Launched by the start-on-boot entry: the window is not focused.
        from_autostart: args.iter().any(|arg| arg == "--autostart"),
    })
}

/// Prints the decoding plan of every page, which is handy to validate a
/// configuration on a headless deployment.
fn print_schedule(config: &AppConfig, path: &Path) {
    let cameras = config.active_cameras();
    println!("configuration : {}", path.display());
    println!("cameras       : {} enabled / {} total", cameras.len(), config.cameras.len());
    println!(
        "backend       : {} (hardware: {})",
        monitor_codec::capabilities().backend,
        monitor_codec::capabilities().hardware
    );

    for layout in GridLayout::ALL {
        let mut scheduler = Scheduler::new(layout, 0, None, cameras.len());
        let pages = scheduler.page_count();
        for page in 0..pages {
            let _ = scheduler.set_page(page);
            let schedule = scheduler.schedule(&cameras);
            let live = schedule
                .plans
                .iter()
                .filter(|plan| !plan.mode.is_suspended())
                .map(|plan| {
                    let name = cameras.get(plan.index).map(|camera| camera.name.as_str()).unwrap_or("?");
                    format!("{name} ({})", plan.mode.label())
                })
                .collect::<Vec<_>>();
            println!(
                "  {:<4} page {}/{}: {} live channel(s) [{}]",
                layout.label(),
                page + 1,
                pages,
                live.len(),
                live.join(", ")
            );
        }
    }
}

/// Reads `--flag value` from the command line.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("xgview=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}
