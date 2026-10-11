//! The command line entry point of XGView.
//!
//! `xgview.exe` is linked as a Windows GUI application, so starting the viewer
//! shows no console window. On the command line that has a price: a shell does
//! not wait for a GUI process, so it prints its prompt before the output has
//! arrived and leaves the cursor where its own bookkeeping put it - at the top
//! of the output rather than after it, with a second prompt mixed in.
//!
//! This binary is a console application, so the shell does wait for it. It runs
//! the viewer installed beside it, copies what the viewer writes to its own
//! standard streams, and ends with the viewer's exit code. Every switch the
//! viewer takes works here, and with none of them the viewer runs in the
//! foreground with its log arriving in this console - which is the one way to
//! watch that log, the GUI having no console of its own.
//!
//! With `--supervise` this is the viewer's supervisor instead of a pipe around a
//! single run: it starts the viewer again when it dies, and takes it down when
//! ctrl-c is pressed here. This side is also where ctrl-c *can* be noticed - the
//! viewer, being a GUI process, is not attached to this console and is never
//! signalled through it - so a supervised viewer is watched from a command line
//! by this binary rather than by the viewer's own `--supervise`, which is for
//! the start-on-boot entry. See `monitor_core::supervise`.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;

use monitor_core::supervise;

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let switch = OsStr::new(supervise::SWITCH);
    let supervising = args.iter().any(|arg| arg == switch);
    // The switch belongs to this process: a viewer that carried it too would be a
    // supervisor of itself, one process deeper, for ever.
    let passed: Vec<OsString> = args.into_iter().filter(|arg| arg != switch).collect();

    let viewer = match viewer_beside_this() {
        Some(viewer) => viewer,
        None => {
            eprintln!("xgview: xgview.exe is not installed beside this program");
            std::process::exit(1);
        }
    };

    if supervising {
        #[cfg(windows)]
        console::install();
        supervise(viewer, &passed);
    }

    let status = run_once(&viewer, &passed);
    std::process::exit(status.code().unwrap_or(1));
}

/// Keeps the viewer running. The shell that ran this command stays here, which is
/// what a supervised viewer wants of it: its log lands in this console.
fn supervise(viewer: PathBuf, args: &[OsString]) -> ! {
    let mut record = supervise::Record::new();
    loop {
        let status = run_once(&viewer, args);
        // Ctrl-c here, which is a stop and not a death to be started again over.
        // On unix there is nothing to ask: the terminal signals the whole
        // foreground group, so this process is going down alongside the viewer
        // and never reaches this line.
        #[cfg(windows)]
        if console::stopping() {
            std::process::exit(status.code().unwrap_or(1));
        }
        match record.watch(status, Instant::now()) {
            // The viewer's own code, so that a shell or a script sees what
            // happened rather than the supervisor's opinion of it.
            supervise::Next::LeaveItDown => std::process::exit(status.code().unwrap_or(1)),
            supervise::Next::Again(delay) => std::thread::sleep(delay),
        }
    }
}

/// Starts the viewer once, copying what it writes to this process's streams, and
/// waits for it.
fn run_once(viewer: &Path, args: &[OsString]) -> ExitStatus {
    let mut child = match Command::new(viewer)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("xgview: cannot start {}: {error}", viewer.display());
            std::process::exit(1);
        }
    };

    // Only the ctrl-c handler reads this, and only on windows, where it is the
    // one thing that can take the viewer down with this process. The field stays
    // because the handler has to reach it from a thread of its own.
    #[cfg(windows)]
    console::watching(&child);

    // Both pipes are drained while the viewer runs. A pipe holds about 64 KiB
    // and its writer blocks once it is full, so reading only after the viewer
    // exits would deadlock as soon as the output is larger than that - and the
    // schedule of a large configuration already is.
    let stdout = child
        .stdout
        .take()
        .map(|pipe| forward(pipe, Box::new(std::io::stdout())));
    let stderr = child
        .stderr
        .take()
        .map(|pipe| forward(pipe, Box::new(std::io::stderr())));

    let status = child.wait();
    for thread in stdout.into_iter().chain(stderr) {
        let _ = thread.join();
    }

    match status {
        Ok(status) => status,
        Err(error) => {
            eprintln!("xgview: cannot wait for {}: {error}", viewer.display());
            std::process::exit(1);
        }
    }
}

/// `xgview.exe` from this program's own directory.
///
/// The two are installed together, and the viewer is not looked up on `PATH`: a
/// different build could be found there, and the output would come from a viewer
/// this command line never asked for.
fn viewer_beside_this() -> Option<PathBuf> {
    let mut viewer = std::env::current_exe().ok()?.parent()?.join("xgview");
    viewer.set_extension(std::env::consts::EXE_EXTENSION);
    if viewer.is_file() {
        Some(viewer)
    } else {
        None
    }
}

/// Copies one of the viewer's pipes to one of this process's streams.
///
/// On a thread of its own: with a single thread reading the two in turn, the
/// pipe that is not being read could fill and block the viewer.
fn forward(
    mut from: impl Read + Send + 'static,
    mut to: Box<dyn Write + Send>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buffer = [0u8; 16 * 1024];
        while let Ok(read) = from.read(&mut buffer) {
            if read == 0 {
                break;
            }
            if to.write_all(&buffer[..read]).is_err() {
                break;
            }
        }
        let _ = to.flush();
    })
}

/// Ctrl-c, which a supervisor has to deal with itself.
///
/// The viewer is a GUI process, so it is not attached to this console and the
/// signal goes nowhere near it: without this, ctrl-c would end the supervisor and
/// leave the viewer running with nobody watching it - which is the opposite of
/// what was asked for.
#[cfg(windows)]
mod console {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

    use windows_sys::Win32::Foundation::{HANDLE, TRUE};
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    use windows_sys::Win32::System::Threading::TerminateProcess;

    /// Set by the handler, which windows runs on a thread of its own.
    static STOPPING: AtomicBool = AtomicBool::new(false);

    /// The viewer's process handle, for the handler to end it with; zero when no
    /// viewer of ours is running.
    static VIEWER: AtomicIsize = AtomicIsize::new(0);

    /// Whether ctrl-c has been pressed in this console.
    pub(super) fn stopping() -> bool {
        STOPPING.load(Ordering::Relaxed)
    }

    /// Hands the handler the viewer it may have to take down.
    pub(super) fn watching(child: &Child) {
        VIEWER.store(child.as_raw_handle() as isize, Ordering::Relaxed);
    }

    pub(super) fn install() {
        // Safety: the handler is `extern "system"`, outlives this call, and reads
        // nothing but the two statics above.
        unsafe { SetConsoleCtrlHandler(Some(handler), TRUE) };
    }

    /// Takes the viewer down and reports the signal as handled, so that this
    /// process is left to finish tidily rather than being ended where it stands.
    unsafe extern "system" fn handler(_kind: u32) -> i32 {
        STOPPING.store(true, Ordering::Relaxed);
        let viewer = VIEWER.load(Ordering::Relaxed);
        if viewer != 0 {
            // Safety: the handle is the child's - this process holds it until
            // the child has been waited for, so it is still a process handle.
            unsafe { TerminateProcess(viewer as HANDLE, 1) };
        }
        TRUE
    }
}
