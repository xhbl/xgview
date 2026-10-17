//! One window per configuration.
//!
//! A second launch of the same configuration does not open a second wall: it
//! finds the first, asks it to come forward, and ends. What counts as "the same"
//! is the configuration file - the lock is named after it and lives beside it -
//! so a viewer who deliberately runs a second wall from another `--config` still
//! can.
//!
//! Two halves, and they are separate. *Claiming* says whether this process is
//! the first, and is a bound socket on Unix and a pair of named kernel objects
//! on Windows. *Asking* is how a later launch hands over: a connection to that
//! socket, or an event set. Bringing the window forward is the owning instance's
//! own work, through egui - the platform knows how, and on a Wayland session
//! there is nothing that can be done about it at all, so a second launch there
//! ends with nothing visible having happened.
//!
//! Nothing here is fatal. A lock that cannot be taken - a directory that is not
//! writable, say - is reported and the viewer starts as it always did, with a
//! second window rather than none.
//!
//! Android is the one platform that takes no lock at all: its activity is
//! declared `singleTask`, so a second launch is delivered to the instance that
//! is already running and never becomes a second process. `run` does not ask
//! there, and what is left of this module is inert.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

/// What a launch found when it asked for the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// No other instance holds it: this one does, for as long as it lives.
    First,
    /// Another instance holds it, and has been asked to come forward. The caller
    /// ends without a window of its own.
    Running,
    /// The lock could not be taken, and not because another instance has it. The
    /// caller starts anyway.
    Unusable(String),
}

/// Asks for the configuration, and holds it if the answer is yes.
pub fn claim(config: &Path) -> Claim {
    match platform::bind(config) {
        platform::Bound::Listening(held) => {
            *lock(&HELD) = Some(held);
            Claim::First
        }
        platform::Bound::Taken => {
            platform::ask_to_come_forward(config);
            Claim::Running
        }
        platform::Bound::Failed(reason) => Claim::Unusable(reason),
    }
}

/// Whether another launch has asked this instance to come forward since this was
/// last called.
pub fn asked_to_come_forward() -> bool {
    lock(&HELD).as_ref().is_some_and(platform::take_ask)
}

/// The claim this process holds, `None` until one is taken.
static HELD: Mutex<Option<platform::Held>> = Mutex::new(None);

/// Locks the static, recovering from a panic that happened while it was held - a
/// poisoned lock here would leave the process unable to read its own claim.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};

    /// The listener a claim is a bound one of.
    pub(super) struct Held(UnixListener);

    /// What a claim found.
    pub(super) enum Bound {
        /// This process has it.
        Listening(Held),
        /// Another process has it.
        Taken,
        /// Nothing could be asked of this path.
        Failed(String),
    }

    /// The socket the lock is: the configuration file's own name with `.sock`,
    /// in the directory the configuration lives in.
    ///
    /// The name is what makes the lock per configuration rather than per
    /// installation - two configurations in one directory get two sockets - and
    /// keeping it beside the file means it lands wherever the configuration was
    /// asked for, `--config` included.
    fn socket(config: &Path) -> PathBuf {
        let mut name = config.file_name().unwrap_or_default().to_os_string();
        name.push(".sock");
        config.with_file_name(name)
    }

    pub(super) fn bind(config: &Path) -> Bound {
        let path = socket(config);
        match UnixListener::bind(&path) {
            Ok(listener) => Bound::Listening(listening(listener)),
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                if UnixStream::connect(&path).is_ok() {
                    // Someone is listening: that is the instance being looked for.
                    return Bound::Taken;
                }
                // The file is there and nothing answers it, so the process that
                // made it is gone - a socket file outlives its owner, which makes
                // this the ordinary path on a second run rather than an accident.
                let _ = std::fs::remove_file(&path);
                match UnixListener::bind(&path) {
                    Ok(listener) => Bound::Listening(listening(listener)),
                    Err(err) => Bound::Failed(err.to_string()),
                }
            }
            Err(err) => Bound::Failed(err.to_string()),
        }
    }

    /// A listener that never blocks the caller looking for an ask.
    fn listening(listener: UnixListener) -> Held {
        let _ = listener.set_nonblocking(true);
        Held(listener)
    }

    /// A connection is the whole of the request; nothing has to be written to it.
    pub(super) fn ask_to_come_forward(config: &Path) {
        let _ = UnixStream::connect(socket(config));
    }

    pub(super) fn take_ask(held: &Held) -> bool {
        let mut asked = false;
        loop {
            match held.0.accept() {
                Ok(_) => asked = true,
                // A busier error than "nothing waiting" is not worth reporting:
                // all this decides is whether a window comes forward.
                Err(_) => return asked,
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::hash::{Hash, Hasher};
    use std::path::Path;

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, OpenEventW, ResetEvent, SetEvent, WaitForSingleObject,
        EVENT_MODIFY_STATE,
    };

    /// The two kernel objects a claim is made of.
    ///
    /// The mutex is the claim itself - creating it with a name that is already
    /// taken is what says another instance is up - and the event is the ask. A
    /// pair rather than a pipe: nothing has to be carried across, only announced,
    /// and the owning instance is already looking once per frame.
    pub(super) struct Held {
        /// Nothing reads this handle: creating the name is the claim, and letting
        /// the handle go would give the claim up.
        _mutex: isize,
        event: isize,
    }

    /// What a claim found.
    pub(super) enum Bound {
        /// This process has it.
        Listening(Held),
        /// Another process has it.
        Taken,
        /// Nothing could be asked of this path.
        Failed(String),
    }

    /// The names the two objects are created under, from the configuration the
    /// claim is for.
    ///
    /// `Local\` rather than `Global\`: one instance per session is what a window
    /// belongs to, and a second viewer at the same machine is another session.
    /// The configuration is folded in by hash, since a path is not a legal object
    /// name and two configurations have to end up with two names.
    fn names(config: &Path) -> (Vec<u16>, Vec<u16>) {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        config.hash(&mut hasher);
        let tag = hasher.finish();
        (wide(&format!("Local\\xgview-{tag:016x}")), wide(&format!("Local\\xgview-{tag:016x}-activate")))
    }

    /// A NUL terminated wide string of the kind the `W` entry points take.
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn bind(config: &Path) -> Bound {
        let (mutex_name, event_name) = names(config);
        // Safety: the call takes two out parameters and a NUL terminated name,
        // and touches no memory of ours; a null attribute block asks for the
        // default security.
        let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr()) };
        if mutex == 0 {
            return Bound::Failed(format!("CreateMutexW: {}", std::io::Error::last_os_error()));
        }
        // Read before anything else can overwrite it: this is what says the name
        // was already taken.
        let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if already {
            // Safety: the handle came from `CreateMutexW` and is not used again.
            unsafe { CloseHandle(mutex) };
            return Bound::Taken;
        }
        // Safety: as above, with a name of ours.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr()) };
        if event == 0 {
            let reason = format!("CreateEventW: {}", std::io::Error::last_os_error());
            // Safety: as above.
            unsafe { CloseHandle(mutex) };
            return Bound::Failed(reason);
        }
        Bound::Listening(Held { _mutex: mutex, event })
    }

    /// Sets the event the owning instance is waiting on.
    pub(super) fn ask_to_come_forward(config: &Path) {
        let (_, event_name) = names(config);
        // Safety: a NUL terminated name and an access mask, with the handle used
        // only for the two calls below.
        let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr()) };
        if event != 0 {
            // Safety: as above.
            unsafe {
                SetEvent(event);
                CloseHandle(event);
            }
        }
    }

    pub(super) fn take_ask(held: &Held) -> bool {
        // Safety: the handle came from `CreateEventW` and is used for these two
        // calls only; a zero timeout makes both of them immediate.
        unsafe {
            if WaitForSingleObject(held.event, 0) == WAIT_OBJECT_0 {
                ResetEvent(held.event);
                return true;
            }
        }
        false
    }
}

#[cfg(all(test, unix, not(target_os = "android")))]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A configuration path nothing else is using, so a test's own socket does
    /// not meet another test's.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xgview-instance-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir.join(name)
    }

    /// The whole of the lock, in one test: the claim is the configuration's, the
    /// ask travels over it, and another configuration is its own lock.
    ///
    /// One test rather than three because the claim is held in a process-wide
    /// static - two of these running side by side would be reading each other's
    /// socket.
    #[test]
    fn a_claim_is_per_configuration_and_carries_the_ask() {
        let wall = scratch("wall.json");
        assert_eq!(claim(&wall), Claim::First, "the first launch takes it");
        assert!(!asked_to_come_forward(), "nothing has been asked of it yet");

        // A second launch of the same configuration: the socket is there and
        // answered, so it is the one that hands over. Asked from this process,
        // which is the one holding the listener - what is under test is the
        // answer, not who gives it.
        assert_eq!(claim(&wall), Claim::Running);

        // That hand-over left an ask waiting: it is read once, and only once.
        assert!(asked_to_come_forward(), "the ask is waiting");
        assert!(!asked_to_come_forward(), "and it waits only once");

        // Another configuration beside it is a lock of its own.
        let other = scratch("other.json");
        assert_eq!(claim(&other), Claim::First);
    }
}
