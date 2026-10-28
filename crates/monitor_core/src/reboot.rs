//! Cross-platform reboot: whether this process may restart the machine, and how.
//!
//! A surveillance wall is often the only thing a machine is for, and a viewer
//! that can reach the settings can reach the power switch. Whether it may is a
//! question of privilege, not of mechanism: the button is drawn only where the
//! answer is yes, so a press never lands on something the system will refuse.
//!
//! | Platform | May reboot | How |
//! |----------|------------|-----|
//! | Windows  | the process token holds `SeShutdownPrivilege` | `shutdown /r /t 0` |
//! | macOS    | root, or the user is in the `admin` or `wheel` group | `osascript … restart` |
//! | Linux    | root, or `systemctl` is on the path | `systemctl reboot` |
//! | Android  | never — `REBOOT` is a system-only permission | — |

use crate::error::Result;
use std::sync::OnceLock;

/// `true` when this process has the privilege to restart the machine.
///
/// Asked where the settings panel is drawn, which is every frame, so the
/// answer is worked out once and held: the privilege does not change while the
/// process is alive.
pub fn is_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(imp::supported)
}

/// Restarts the machine.
///
/// Confirmed by the caller first: this is the action, not the question.
pub fn reboot() -> Result<()> {
    imp::reboot()
}

#[cfg(windows)]
mod imp {
    use std::process::Command;

    use super::*;
    use crate::error::CoreError;

    // advapi32 and kernel32 are linked by the standard library on Windows, so
    // declaring the functions needed here is cheaper than a dependency for it.
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn OpenProcessToken(handle: isize, access: u32, token: *mut isize) -> i32;
        fn CloseHandle(handle: isize) -> i32;
        fn LookupPrivilegeValueW(system: *const u16, name: *const u16, luid: *mut u64) -> i32;
        fn GetTokenInformation(
            token: isize,
            class: u32,
            buf: *mut u8,
            len: u32,
            ret: *mut u32,
        ) -> i32;
    }

    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_PRIVILEGES_CLASS: u32 = 3;

    pub(super) fn supported() -> bool {
        let Some(wanted) = lookup_shutdown_luid() else {
            return false;
        };
        let mut token = 0isize;
        // Safety: `GetCurrentProcess` returns a pseudo-handle that is always
        // valid; `OpenProcessToken` writes the handle only on success.
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if ok == 0 {
            return false;
        }
        let found = token_holds(token, wanted);
        // Safety: the handle was opened above, and only on success.
        unsafe { CloseHandle(token) };
        found
    }

    pub(super) fn reboot() -> Result<()> {
        match Command::new("shutdown").args(["/r", "/t", "0"]).status() {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(CoreError::config(format!("shutdown exited {status}"))),
            Err(err) => Err(CoreError::config(format!("cannot run shutdown: {err}"))),
        }
    }

    /// The LUID of `SeShutdownPrivilege`, or `None` if it cannot be looked up.
    fn lookup_shutdown_luid() -> Option<u64> {
        let name: Vec<u16> = "SeShutdownPrivilege\0".encode_utf16().collect();
        let mut luid = 0u64;
        // Safety: a null system name means the local machine, and `name` is a
        // nul-terminated UTF-16 string. The LUID is an 8-byte value laid out
        // as a `u64`, written only on success.
        let ok = unsafe { LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) };
        (ok != 0).then_some(luid)
    }

    /// Whether `wanted` is among the privileges this token holds.
    fn token_holds(token: isize, wanted: u64) -> bool {
        let mut len = 0u32;
        // Safety: a null buffer with a zero length is the documented way to ask
        // for the size; nothing is written.
        unsafe {
            GetTokenInformation(token, TOKEN_PRIVILEGES_CLASS, std::ptr::null_mut(), 0, &mut len);
        }
        if len == 0 {
            return false;
        }
        let mut buf = vec![0u8; len as usize];
        let mut ret = 0u32;
        // Safety: the buffer is `len` bytes, as the previous call asked for.
        let ok = unsafe {
            GetTokenInformation(token, TOKEN_PRIVILEGES_CLASS, buf.as_mut_ptr(), len, &mut ret)
        };
        if ok == 0 || ret == 0 {
            return false;
        }
        // `TOKEN_PRIVILEGES` is `{ DWORD count; LUID_AND_ATTRIBUTES[count] }`,
        // and `LUID_AND_ATTRIBUTES` is `{ LUID (8 bytes), DWORD (4 bytes) }` —
        // 12 bytes per entry, packed as the platform's ABI lays them out.
        let count = u32::from_ne_bytes(buf[0..4].try_into().unwrap()) as usize;
        const STRIDE: usize = 12;
        for i in 0..count {
            let off = 4 + i * STRIDE;
            if off + 8 > buf.len() {
                break;
            }
            let luid = u64::from_ne_bytes(buf[off..off + 8].try_into().unwrap());
            if luid == wanted {
                return true;
            }
        }
        false
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::process::Command;

    use super::*;
    use crate::error::CoreError;

    pub(super) fn supported() -> bool {
        if is_root() {
            return true;
        }
        // An administrator is in `admin` (macOS) or `wheel` (the BSD name for
        // the same group); either may reach `shutdown`, which is what the
        // AppleScript below resolves to.
        let Ok(output) = Command::new("id").arg("-Gn").output() else {
            return false;
        };
        let groups = String::from_utf8_lossy(&output.stdout);
        groups.split_whitespace().any(|g| g == "admin" || g == "wheel")
    }

    pub(super) fn reboot() -> Result<()> {
        // `System Events`' `restart` is the user-facing way: an administrator
        // account is allowed it, a managed one is not, and a viewer started
        // under launchd has no frontmost application to confirm for it, so this
        // goes straight through.
        match Command::new("osascript")
            .arg("-e")
            .arg("tell application \"System Events\" to restart")
            .status()
        {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(CoreError::config(format!("osascript exited {status}"))),
            Err(err) => Err(CoreError::config(format!("cannot run osascript: {err}"))),
        }
    }

    /// `true` when this process is running as root.
    fn is_root() -> bool {
        Command::new("id")
            .arg("-u")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
            .unwrap_or(false)
    }
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
mod imp {
    use std::process::Command;

    use super::*;
    use crate::error::CoreError;

    pub(super) fn supported() -> bool {
        is_root() || systemctl_present()
    }

    pub(super) fn reboot() -> Result<()> {
        match Command::new("systemctl").arg("reboot").status() {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(CoreError::config(format!("systemctl exited {status}"))),
            Err(err) => Err(CoreError::config(format!("cannot run systemctl: {err}"))),
        }
    }

    /// `true` when this process is running as root.
    fn is_root() -> bool {
        Command::new("id")
            .arg("-u")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
            .unwrap_or(false)
    }

    /// `true` when `systemctl` is on the path and answers.
    ///
    /// A desktop session with polkit grants `systemctl reboot` to an
    /// unprivileged user through an authorisation prompt; a session without it
    /// (a container, a WSL session) does not, and the button is left out.
    fn systemctl_present() -> bool {
        Command::new("systemctl")
            .arg("--version")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

#[cfg(target_os = "android")]
mod imp {
    use super::*;
    use crate::error::CoreError;

    pub(super) fn supported() -> bool {
        false
    }

    pub(super) fn reboot() -> Result<()> {
        Err(CoreError::unsupported("reboot is not available on Android"))
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    use super::*;
    use crate::error::CoreError;

    pub(super) fn supported() -> bool {
        false
    }

    pub(super) fn reboot() -> Result<()> {
        Err(CoreError::unsupported("reboot is not available on this platform"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A platform that cannot reboot must say so rather than reaching for a
    /// command that is not there.
    #[test]
    fn an_unsupported_platform_refuses_to_reboot() {
        if !is_supported() {
            assert!(reboot().is_err());
        }
    }
}