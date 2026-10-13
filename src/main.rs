//! XGView entry point.
//!
//! A plain `cargo run` starts the viewer with the configuration found in the
//! platform configuration directory. Deployment helpers are exposed as
//! command line switches so that the Windows / Android install scripts can
//! register the start-on-boot entry without any UI interaction.

// A Windows GUI application: the loader creates no console window for the
// process, so a double-click, a shortcut or the start-on-boot entry shows
// nothing at all before the viewer's own window. The command line still
// prints - see `setup_console`.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::Result;
use monitor_core::autostart;
use monitor_core::config::AppConfig;
use monitor_core::layout::GridLayout;
use monitor_core::scheduler::Scheduler;
use monitor_core::supervise;
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
  --supervise          Keep the viewer running: start it again if it dies
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

    // A supervisor opens nothing itself: it starts the viewer - this same program
    // without this switch - and watches it. Placed after the switches that print
    // and exit, so that a help or a schedule asked for on a supervised line is
    // still just that.
    if args.iter().any(|arg| arg == supervise::SWITCH) {
        supervise_viewer(&args);
    }

    let fullscreen = if args.iter().any(|arg| arg == "--windowed") {
        false
    } else {
        args.iter().any(|arg| arg == "--fullscreen") || config.start_fullscreen
    };

    tracing::info!(
        target: "xgview",
        path = %config_path.display(),
        cameras = config.group_camera_count(config.active_group),
        group = config.active_group + 1,
        layout = config.group().layout.label(),
        "starting {APP_DISPLAY_NAME} {}",
        env!("CARGO_PKG_VERSION")
    );

    match monitor_gui::run(RunOptions {
        config,
        config_path: Some(config_path),
        fullscreen,
        // Launched by the start-on-boot entry: the window is not focused.
        from_autostart: args.iter().any(|arg| arg == "--autostart"),
        // A supervisor leaves its mark on the child so the viewer knows to keep
        // the wall from being left by accident; see `monitor_core::supervise`.
        supervised: std::env::var(supervise::ENV).is_ok(),
        // Language packs live under the configuration directory here; only
        // Android names a second place a file transfer can reach.
        extra_lang_dir: None,
    }) {
        Ok(()) => Ok(()),
        // No renderer to be had on this machine, which is one answer a restart
        // cannot change. Saying so in the exit code is what lets a supervisor
        // stop asking; see `monitor_core::supervise` and `monitor_gui::CannotDraw`.
        Err(err) if err.downcast_ref::<monitor_gui::CannotDraw>().is_some() => {
            tracing::error!(target: "xgview", %err, "cannot draw here, and starting again would not change that");
            std::process::exit(supervise::CANNOT_RUN);
        }
        Err(err) => Err(err),
    }
}

/// Keeps the viewer running: starts it, waits, and starts it again when it dies
/// without having been asked to stop.
///
/// The viewer is this program without `--supervise`, so that switch is the whole
/// difference between the two roles - and it is not handed down, or the child
/// would be a supervisor of itself, one process deeper, for ever.
///
/// Windows note: this process is the GUI subsystem one, so it has no console and
/// ctrl-c never reaches it. A viewer watched from a command line is watched by
/// `xgview.com` instead, which does have one; see the `xgview-cli` binary.
fn supervise_viewer(args: &[String]) -> ! {
    let viewer = match std::env::current_exe() {
        Ok(viewer) => viewer,
        Err(err) => {
            tracing::error!(target: "xgview", %err, "cannot find the viewer to supervise");
            std::process::exit(1);
        }
    };
    let passed: Vec<&String> = args
        .iter()
        .filter(|arg| arg.as_str() != supervise::SWITCH)
        .collect();

    let mut record = supervise::Record::new();
    loop {
        let status = match Command::new(&viewer)
            .args(&passed)
            .env(supervise::ENV, "1")
            .status()
        {
            Ok(status) => status,
            // The viewer is not there to be started at all, which is not
            // something waiting would fix.
            Err(err) => {
                tracing::error!(target: "xgview", %err, "cannot start the viewer");
                std::process::exit(1);
            }
        };
        match record.watch(status, Instant::now()) {
            // The viewer's own code, so that whoever started the supervisor -
            // a start-on-boot entry, a shell, systemd - sees what happened.
            supervise::Next::LeaveItDown => std::process::exit(status.code().unwrap_or(1)),
            supervise::Next::Again(delay) => std::thread::sleep(delay),
        }
    }
}

/// Prints the decoding plan of every page of every group, which is handy to
/// validate a configuration on a headless deployment.
fn print_schedule(config: &AppConfig, path: &Path) {
    cli_print(&format!("configuration : {}\n", path.display()));
    cli_print(&format!(
        "backend       : {} (hardware: {})\n",
        monitor_codec::capabilities().backend,
        monitor_codec::capabilities().hardware
    ));

    for (index, group) in config.groups.iter().enumerate() {
        let held: Vec<_> = config
            .cameras
            .iter()
            .filter(|camera| usize::from(camera.group) == index)
            .cloned()
            .collect();
        let cameras: Vec<_> = held.iter().filter(|camera| camera.enabled).cloned().collect();
        let title = if group.name.trim().is_empty() {
            format!("group {}", index + 1)
        } else {
            format!("group {} ({})", index + 1, group.name.trim())
        };
        let on_the_wall = if index == config.active_group { ", on the wall" } else { "" };
        cli_print(&format!(
            "\n{title}{on_the_wall}: {} enabled / {} total, grid {}\n",
            cameras.len(),
            held.len(),
            group.layout.label()
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
}

/// Reads `--flag value` from the command line.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

/// Write to stdout and flush. `\n` alone is handed over: a console ends its own
/// lines, and a redirected file is written as the bytes were given.
fn cli_print(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// Opens the console window `--console` asks for, on a Windows build that has
/// none of its own.
///
/// The executable is linked as a GUI application (the `windows_subsystem`
/// attribute at the top of this file), so nothing is on screen before the
/// viewer's own window: a console subsystem binary gets its console window from
/// the loader, before `main` runs, and closing it from here only ever closes a
/// window that has already been seen - which is the flash this avoids.
///
/// It is also why nothing is done here for the switches that print: a GUI
/// process is not given the console of the shell it was launched from, so their
/// output would have nowhere to go. The command line is served by `xgview.com`
/// beside the viewer, which a shell finds before `xgview.exe` and waits for -
/// see the `xgview-cli` binary. A standard output that is already a file or a
/// pipe is written to as it always was.
///
/// `--console` is the one case left: it asks for a console window to watch the
/// log in, so one is attached to - the shell's, if the shell has one - or made,
/// for a double-click. A standard output that already has somewhere to go is
/// left alone, which is what keeps `xgview --console > log.txt` a file.
#[cfg(windows)]
fn setup_console(args: &[String]) {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AllocConsole, AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS,
        STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    if !args.iter().any(|a| a == "--console") {
        return;
    }

    // Redirected to a file or a pipe: that handle is the one to write to.
    let stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if stdout != 0 && stdout != INVALID_HANDLE_VALUE {
        return;
    }

    unsafe {
        // The console of the shell that ran us, which needs no window of its
        // own. With nothing to attach to there is a console window to make.
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 && AllocConsole() == 0 {
            return;
        }
        // Neither call hands over handles of its own: the console is opened by
        // name instead, and the standard handles are pointed at it.
        let conout: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        let handle = CreateFileW(
            conout.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            0,
        );
        if handle == INVALID_HANDLE_VALUE {
            return;
        }
        SetStdHandle(STD_OUTPUT_HANDLE, handle);
        SetStdHandle(STD_ERROR_HANDLE, handle);
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
