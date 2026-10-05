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
use monitor_core::APP_DISPLAY_NAME;
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
  --console            Open a console window for log output (Windows only)
  -h, --help           Print this help
  -V, --version        Print the version
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    setup_console(&args);
    init_logging();

    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        cli_print(HELP);
        return Ok(());
    }
    if args.iter().any(|arg| arg == "-V" || arg == "--version") {
        // Authors and version come from the manifest, the same source the about
        // panel reads. `(c)` rather than `©`: a redirected log or a terminal
        // without the glyph should still read.
        cli_print(&format!(
            "{APP_DISPLAY_NAME} v{} by {} (c) {}\n",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_AUTHORS"),
            monitor_core::copyright_years()
        ));
        return Ok(());
    }

    if args.iter().any(|arg| arg == "--install-autostart") {
        monitor_i18n::init("en", &[]);
        autostart::set_enabled(true)?;
        cli_print(&format!(
            "start-on-boot registered ({})\n",
            monitor_i18n::tr(autostart::mechanism())
        ));
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--remove-autostart") {
        autostart::set_enabled(false)?;
        cli_print("start-on-boot registration removed\n");
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
        // The desktop opens a file dialog for the About tab's export / import.
        export_dir: None,
    })
}

/// Prints the decoding plan of every page, which is handy to validate a
/// configuration on a headless deployment.
fn print_schedule(config: &AppConfig, path: &Path) {
    let cameras = config.active_cameras();
    cli_print(&format!("configuration : {}\n", path.display()));
    cli_print(&format!(
        "cameras       : {} enabled / {} total\n",
        cameras.len(),
        config.cameras.len()
    ));
    cli_print(&format!(
        "backend       : {} (hardware: {})\n",
        monitor_codec::capabilities().backend,
        monitor_codec::capabilities().hardware
    ));

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
            cli_print(&format!(
                "  {:<4} page {}/{}: {} live channel(s) [{}]\n",
                layout.label(),
                page + 1,
                pages,
                live.len(),
                live.join(", ")
            ));
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

/// Write to stdout and flush. Under the console subsystem `println!` handles
/// line endings correctly, so this is a plain write + flush.
fn cli_print(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// Release the console window when launching the GUI without `--console`.
///
/// The binary runs under the console subsystem (no `windows_subsystem`
/// attribute), so `println!` works normally and `\n` is translated to
/// `\r\n` by the console. When starting the GUI — no CLI switch, no
/// `--console` — call `FreeConsole` to close the console window. There is a
/// brief flash on double-click launch before `FreeConsole` runs, but CLI
/// output is always correct.
#[cfg(windows)]
fn setup_console(args: &[String]) {
    use windows_sys::Win32::System::Console::FreeConsole;

    let want_console = args.iter().any(|a| a == "--console");
    let is_cli = args.iter().any(|a| {
        matches!(
            a.as_str(),
            "-h" | "--help" | "-V" | "--version"
                | "--install-autostart" | "--remove-autostart" | "--print-schedule"
        )
    });
    if !want_console && !is_cli {
        unsafe {
            FreeConsole();
        }
    }
}

#[cfg(not(windows))]
fn setup_console(_args: &[String]) {}

fn init_logging() {
    use std::io::IsTerminal;
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("xgview=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        // Colour is for a terminal. Redirected to a file the escape sequences
        // are not decoration but content: they land in the middle of every field
        // and make the log unsearchable, so a log meant to be read back gets
        // none of them.
        .with_ansi(std::io::stdout().is_terminal())
        .init();
}
