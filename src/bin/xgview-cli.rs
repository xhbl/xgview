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

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn main() {
    let viewer = match viewer_beside_this() {
        Some(viewer) => viewer,
        None => {
            eprintln!("xgview: xgview.exe is not installed beside this program");
            std::process::exit(1);
        }
    };

    let mut child = match Command::new(&viewer)
        .args(std::env::args_os().skip(1))
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
        // The viewer's own code: a non-zero one is what the install scripts
        // check for a registration that did not happen.
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
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
