//! Reading media keys from the Linux input layer.
//!
//! A kiosk has no desktop to route media keys to us: the display server either
//! grabs them or translates them into key names egui does not understand, and
//! the remote's zoom keys have keycodes above X11's eight-bit limit. Reading
//! `/dev/input` directly avoids all of that - the events are the kernel's own,
//! before any window system has seen them.
//!
//! The reader never grabs a device. A grab would take the keys away from X11
//! and anything else watching, and on this machine every node is already held
//! by another process; a plain reader still receives every event. It also
//! means the window need not have focus - a wall is often left with the
//! pointer over nothing at all.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use evdev::{Device, EventType, InputEvent};
use rustix::event::{PollFd, PollFlags, Timespec};

use crate::media::{DeviceGroup, MediaCommand, MediaEvent, MediaTx};

/// How long the reader waits for input before looking for new or gone devices.
/// `evdev::enumerate()` is a one-shot listing; it says nothing about a receiver
/// plugged in afterwards.
const RESCAN: Duration = Duration::from_secs(1);

/// Two reports of the same code from one physical device within this window
/// are one press. Nothing in the field data suggests it happens, but some
/// remotes present a key on more than one of their nodes, and a single press
/// must not turn two steps of volume. The table is per physical device, not
/// per node, so the two nodes of one remote share it.
const DEDUPE: Duration = Duration::from_millis(40);

/// What one open input node is known by.
struct Opened {
    group: DeviceGroup,
}

/// Starts the reader thread. A failure to spawn is logged, not fatal: media
/// keys are a convenience, and the wall must still come up without them.
pub fn spawn(ctx: egui::Context, tx: MediaTx) {
    match std::thread::Builder::new().name("xgview-media-evdev".to_string()).spawn(move || {
        run(ctx, tx)
    }) {
        Ok(_) => {}
        Err(err) => tracing::warn!(target: "xgview::media", %err, "cannot start the media key reader"),
    }
}

/// Whether a desktop session is present to take the volume keys.
///
/// A kiosk starts X itself from xinit and names no desktop; a GNOME or KDE
/// session sets one of the XDG desktop variables. Where a desktop is up it
/// already handles volume and mute, and this reader does not grab the devices -
/// so acting on the same events would move the volume twice. Leaving them to
/// the desktop is the "system first" rule the other platforms follow.
///
/// `XGVIEW_MEDIA_VOLUME` overrides the guess for a process started outside its
/// desktop session (from SSH, say), where the session variables are not
/// inherited: `system` leaves the keys to the desktop, `app` takes them here.
/// Checked once at start-up: a process does not change session while it runs.
fn desktop_owns_volume() -> bool {
    let override_ = std::env::var("XGVIEW_MEDIA_VOLUME").unwrap_or_default();
    let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let desktop = ["XDG_CURRENT_DESKTOP", "XDG_SESSION_DESKTOP", "DESKTOP_SESSION"]
        .iter()
        .map(|name| std::env::var(name).unwrap_or_default())
        .find(|value| !value.trim().is_empty())
        .unwrap_or_default();
    let owns = desktop_owns_volume_from(&override_, &session_type, &desktop);
    if !override_.is_empty() {
        tracing::info!(target: "xgview::media", source = "XGVIEW_MEDIA_VOLUME", value = %override_, system = owns,
            "volume and mute ownership set by the environment");
    } else if owns {
        tracing::info!(target: "xgview::media", desktop = %desktop, session_type = %session_type,
            "desktop session present, leaving volume and mute to the system");
    } else {
        tracing::info!(target: "xgview::media", desktop = %desktop, session_type = %session_type,
            "no desktop session, volume and mute come from the remote");
    }
    owns
}

/// The decision itself, separated from the environment so it can be tested
/// without mutating process-wide variables in parallel tests.
fn desktop_owns_volume_from(override_: &str, session_type: &str, desktop: &str) -> bool {
    if override_.eq_ignore_ascii_case("system") {
        return true;
    }
    if override_.eq_ignore_ascii_case("app") {
        return false;
    }
    // A tty session is how SSH and console logins arrive, including a shell
    // opened into a machine that happens to be running a desktop elsewhere.
    // Its variables say nothing about who handles the volume keys.
    !desktop.trim().is_empty() && session_type.trim() != "tty"
}

fn run(ctx: egui::Context, tx: MediaTx) {
    let desktop_volume = desktop_owns_volume();
    let mut devices: HashMap<PathBuf, (Device, Opened)> = HashMap::new();
    let mut dedupe: HashMap<DeviceGroup, HashMap<u16, Instant>> = HashMap::new();
    let mut last_rescan = Instant::now() - RESCAN;
    let _ = rescan(&mut devices, &tx, &ctx);

    loop {
        // Each path and its fd come from one walk of the map, so no ordering
        // assumption is needed. HUP/ERR/NVAL count as ready too: a
        // disconnected device reports those without IN, and leaving them
        // unread would spin the poll at full speed.
        let mut ready: Vec<PathBuf> = Vec::new();
        let mut hung: Vec<PathBuf> = Vec::new();
        {
            // One walk builds both vectors in the same order; `devices` is
            // not touched between them, so index i in each refers to the same
            // device.
            let mut paths: Vec<PathBuf> = Vec::with_capacity(devices.len());
            let mut fds: Vec<PollFd<'_>> = Vec::with_capacity(devices.len());
            for (path, (device, _)) in devices.iter() {
                paths.push(path.clone());
                fds.push(PollFd::new(
                    device,
                    PollFlags::IN | PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL,
                ));
            }
            match rustix::event::poll(&mut fds, Some(&Timespec { tv_sec: 1, tv_nsec: 0 })) {
                Ok(0) => {}
                Ok(_) => {
                    for (path, fd) in paths.iter().zip(fds.iter()) {
                        let flags = fd.revents();
                        if flags.contains(PollFlags::ERR)
                            || flags.contains(PollFlags::HUP)
                            || flags.contains(PollFlags::NVAL)
                        {
                            // A hung fd is level-triggered; if fetch_events
                            // still returns Ok(empty) it must be removed or
                            // the next poll spins at full speed.
                            hung.push(path.clone());
                        }
                        if flags.contains(PollFlags::IN) {
                            ready.push(path.clone());
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(target: "xgview::media", %err, "cannot poll input devices");
                    std::thread::sleep(RESCAN);
                }
            }
        }

        let mut failed: Vec<PathBuf> = Vec::new();
        for path in ready {
            let Some((device, opened)) = devices.get_mut(&path) else {
                continue;
            };
            match device.fetch_events() {
                Ok(events) => {
                    let last = dedupe.entry(opened.group.clone()).or_default();
                    for event in events {
                        handle_event(event, &opened.group, last, &tx, &ctx, desktop_volume);
                    }
                }
                Err(err) if is_gone(&err) => {
                    // ENODEV/EIO: the receiver was unplugged. Take the fd out
                    // and tell the UI, so a key held on it stops repeating.
                    tracing::info!(target: "xgview::media", %err, path = %path.display(), "dropping input device");
                    failed.push(path);
                }
                Err(err) => {
                    // EAGAIN/EINTR and the like: poll said ready but the
                    // device had nothing for us this time. Keep the fd.
                    tracing::debug!(target: "xgview::media", %err, path = %path.display(), "transient input read error");
                }
            }
        }
        for path in hung {
            if !failed.contains(&path) {
                failed.push(path);
            }
        }
        for path in failed {
            if let Some((_, opened)) = devices.remove(&path) {
                dedupe.remove(&opened.group);
                let _ = tx.send(MediaEvent::DeviceLost(opened.group));
                ctx.request_repaint();
            }
        }

        // Enumerating input nodes opens every candidate; with a key held the
        // loop runs at its repeat rate, so only rescan on a slow tick.
        if last_rescan.elapsed() >= RESCAN {
            last_rescan = Instant::now();
            let removed = rescan(&mut devices, &tx, &ctx);
            for group in removed {
                dedupe.remove(&group);
            }
        }
    }
}

/// Whether a read error means the device is gone rather than merely had
/// nothing to give this time.
fn is_gone(err: &std::io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(19) // ENODEV
            | Some(5) // EIO
            | Some(9) // EBADF
    )
}

/// Opens devices that appeared, drops those that vanished, and reports the
/// latter so the UI stops any key still held on them.
fn rescan(
    devices: &mut HashMap<PathBuf, (Device, Opened)>,
    tx: &MediaTx,
    ctx: &egui::Context,
) -> Vec<DeviceGroup> {
    // `enumerate` yields (path, device): the path is the node, the device the
    // handle, and keeping both lets us notice an eventN recycled for a
    // different receiver.
    let present: Vec<(PathBuf, Device)> = evdev::enumerate().collect();
    let paths: HashSet<PathBuf> = present.iter().map(|(path, _)| path.clone()).collect();

    // A node no longer listed is gone. A same-port replug keeps its `eventN`
    // path and ids, but the old fd will report HUP/a read error in the main
    // loop, which removes it and sends DeviceLost; the next rescan then opens
    // the fresh node at the same path. No inode bookkeeping is needed.
    let gone: Vec<PathBuf> = devices
        .keys()
        .filter(|path| !paths.contains(*path))
        .cloned()
        .collect();
    let mut removed = Vec::new();
    for path in gone {
        if let Some((_, opened)) = devices.remove(&path) {
            tracing::info!(target: "xgview::media", path = %path.display(), "input device removed");
            let _ = tx.send(MediaEvent::DeviceLost(opened.group.clone()));
            ctx.request_repaint();
            removed.push(opened.group);
        }
    }

    for (path, candidate) in present {
        if devices.contains_key(&path) {
            continue;
        }
        let group = DeviceGroup::named(format!(
            "{}:{:04x}:{:04x}",
            candidate
                .physical_path()
                .map(phys_base)
                .unwrap_or("unknown"),
            candidate.input_id().vendor(),
            candidate.input_id().product(),
        ));
        let device = candidate;
        if let Err(err) = device.set_nonblocking(true) {
            tracing::warn!(target: "xgview::media", %err, path = %path.display(), "cannot make input device nonblocking");
            continue;
        }
        tracing::info!(target: "xgview::media", path = %path.display(), group = group.as_str(), "watching input device");
        devices.insert(path, (device, Opened { group }));
    }
    removed
}

/// The phys path names one receiver with several nodes: `.../input0` and
/// `.../input1` belong to the same remote. Strip the node suffix so the group
/// is per physical device.
fn phys_base(phys: &str) -> &str {
    match phys.rfind("/input") {
        Some(index) => &phys[..index],
        None => phys,
    }
}

/// Keycodes we own. Everything else is left alone.
fn command_for(code: u16) -> Option<(MediaCommand, bool)> {
    let command = match code {
        113 => MediaCommand::Mute,
        114 => MediaCommand::VolDown,
        115 => MediaCommand::VolUp,
        163 => MediaCommand::NextEvent,
        164 => MediaCommand::PlayPause,
        165 => MediaCommand::PrevEvent,
        168 => MediaCommand::StepBack,
        208 => MediaCommand::StepForward,
        418 => MediaCommand::PictureZoomIn,
        419 => MediaCommand::PictureZoomOut,
        // The keyboard's +/- keys belong to egui's text and picture zoom;
        // taking them here would double every press.
        69 | 70 | 81 | 156 | 157 => return None,
        _ => return None,
    };
    Some((command, command.repeats_while_held()))
}

/// The three volume codes go to the desktop when one owns them; every other
/// media key stays with us. Kept apart from the event reader so the rule can be
/// tested without a kernel device.
fn volume_belongs_to_desktop(desktop_volume: bool, code: u16) -> bool {
    // KEY_MUTE / KEY_VOLUMEDOWN / KEY_VOLUMEUP are consecutive in the kernel's
    // input-event-codes.h, so the range names exactly the three volume keys.
    desktop_volume && matches!(code, 113..=115)
}

fn handle_event(
    event: InputEvent,
    group: &DeviceGroup,
    last: &mut HashMap<u16, Instant>,
    tx: &MediaTx,
    ctx: &egui::Context,
    desktop_volume: bool,
) {
    if event.event_type() != EventType::KEY {
        return;
    }
    let code = event.code();
    // Mute and the two volume steps are the desktop's when one is running.
    // Drop both ends of a hold so a key released after the wall opened cannot
    // leave a repeat behind; the desktop sees the same kernel events anyway.
    if volume_belongs_to_desktop(desktop_volume, code) {
        return;
    }
    let Some((command, holdable)) = command_for(code) else {
        return;
    };
    let value = event.value();
    // 2 is the kernel's auto-repeat. The remotes in use do not generate it,
    // and where it does exist the UI owns the cadence, so ignore it.
    if value != 0 && value != 1 {
        return;
    }

    let now = Instant::now();
    if value == 1 {
        if let Some(previous) = last.get(&code) {
            if now.duration_since(*previous) < DEDUPE {
                return;
            }
        }
        last.insert(code, now);
    }

    let event = if holdable {
        MediaEvent::Hold {
            group: group.clone(),
            command,
            pressed: value == 1,
            at: now,
        }
    } else if value == 1 {
        MediaEvent::Command(command)
    } else {
        return;
    };
    if tx.send(event).is_ok() {
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_codes_map_to_actions() {
        assert_eq!(command_for(113), Some((MediaCommand::Mute, false)));
        assert_eq!(command_for(114), Some((MediaCommand::VolDown, true)));
        assert_eq!(command_for(115), Some((MediaCommand::VolUp, true)));
        assert_eq!(command_for(163), Some((MediaCommand::NextEvent, false)));
        assert_eq!(command_for(164), Some((MediaCommand::PlayPause, false)));
        assert_eq!(command_for(165), Some((MediaCommand::PrevEvent, false)));
        assert_eq!(command_for(168), Some((MediaCommand::StepBack, false)));
        assert_eq!(command_for(208), Some((MediaCommand::StepForward, false)));
        assert_eq!(command_for(418), Some((MediaCommand::PictureZoomIn, true)));
        assert_eq!(command_for(419), Some((MediaCommand::PictureZoomOut, true)));
    }

    #[test]
    fn desktop_volume_detection_covers_kiosk_desktop_and_ssh() {
        // NVR: xinit names no desktop and has no session type.
        assert!(!desktop_owns_volume_from("", "", ""));
        // GNOME on Wayland or X11 from its own session.
        assert!(desktop_owns_volume_from("", "wayland", "ubuntu:GNOME"));
        assert!(desktop_owns_volume_from("", "x11", "KDE"));
        // Only XDG_SESSION_DESKTOP set still counts.
        assert!(desktop_owns_volume_from("", "wayland", "xfce"));
        // SSH into a desktop box is a tty: it cannot tell who owns the keys.
        assert!(!desktop_owns_volume_from("", "tty", "ubuntu:GNOME"));
        // Explicit override wins in either direction.
        assert!(desktop_owns_volume_from("system", "tty", ""));
        assert!(!desktop_owns_volume_from("app", "wayland", "ubuntu:GNOME"));
        // An unrecognised override falls through to the session variables.
        assert!(desktop_owns_volume_from("maybe", "wayland", "ubuntu:GNOME"));
    }

    #[test]
    fn a_desktop_takes_all_three_volume_keys_and_nothing_else() {
        for code in [113, 114, 115] {
            assert!(volume_belongs_to_desktop(true, code), "code {code} is the desktop's");
        }
        // On a kiosk the same keys are ours; with or without a desktop, the
        // transport and zoom keys never belong to the desktop under this rule.
        for code in [113, 114, 115] {
            assert!(!volume_belongs_to_desktop(false, code), "code {code} is ours on a kiosk");
        }
        for code in [163, 164, 165, 168, 208, 418, 419] {
            assert!(!volume_belongs_to_desktop(true, code), "code {code} is never the volume");
        }
    }

    #[test]
    fn keyboard_plus_and_minus_are_not_ours() {
        for code in [69, 70, 81, 156, 157] {
            assert_eq!(command_for(code), None, "code {code} belongs to the keyboard");
        }
    }

    #[test]
    fn phys_path_strips_the_node_number() {
        assert_eq!(
            phys_base("usb-0000:00:14.0-1.3/input0"),
            "usb-0000:00:14.0-1.3"
        );
        assert_eq!(phys_base("usb-0000:00:14.0-1.3/input1"), phys_base("usb-0000:00:14.0-1.3/input0"));
        assert_eq!(phys_base("no-node-suffix"), "no-node-suffix");
    }
}
