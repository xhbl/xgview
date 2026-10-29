//! Windows: the registry for the version, `GlobalMemoryStatusEx` for memory and
//! `GetSystemTimes` for processor load.

use winreg::enums::HKEY_LOCAL_MACHINE;
use winreg::RegKey;
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::GetSystemTimes;

pub(super) struct Platform {
    os: String,
    /// Busy and total ticks at the previous reading.
    cpu: Option<(u64, u64)>,
}

impl Platform {
    pub(super) fn new() -> Self {
        Self { os: os_name(), cpu: cpu_times() }
    }

    pub(super) fn os(&self) -> &str {
        &self.os
    }

    /// Total and used physical memory, both in bytes.
    ///
    /// "Used" is total less available, which is the figure Task Manager's
    /// percentage is built from; standby and cached memory counts as available.
    pub(super) fn memory(&self) -> Option<(u64, u64)> {
        let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: `status` is a `MEMORYSTATUSEX` with its own length filled in,
        // which is what the call expects.
        let result = unsafe { GlobalMemoryStatusEx(&mut status) };
        if result == 0 {
            return None;
        }
        Some((status.ullTotalPhys, status.ullTotalPhys.saturating_sub(status.ullAvailPhys)))
    }

    pub(super) fn cpu(&mut self) -> Option<f32> {
        let now = cpu_times()?;
        super::cpu_between(self.cpu.replace(now)?, now)
    }
}

/// Busy and total 100 ns ticks since boot.
///
/// `GetSystemTimes` reports idle, kernel and user time, and the kernel's count
/// already includes the idle one, so busy is kernel + user - idle.
fn cpu_times() -> Option<(u64, u64)> {
    let mut idle = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut kernel = idle;
    let mut user = idle;
    // SAFETY: the three are plain `FILETIME` outputs, one per argument.
    let result = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
    if result == 0 {
        return None;
    }
    let total = ticks(kernel) + ticks(user);
    let busy = total.saturating_sub(ticks(idle));
    Some((busy, total))
}

fn ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// "Windows 11 Pro 26H2", from the version block of the registry.
///
/// `ProductName` still says "Windows 10" on Windows 11 - the value was never
/// updated - so the build number decides which it is. `DisplayVersion` is the
/// release Windows itself shows ("26H2"); a machine that predates it is named by
/// its build number instead.
fn os_name() -> String {
    let Ok(key) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
    else {
        return "Windows".to_string();
    };
    let product: String = key.get_value("ProductName").unwrap_or_default();
    let build: u32 = key
        .get_value::<String, _>("CurrentBuildNumber")
        .ok()
        .and_then(|build| build.trim().parse().ok())
        .unwrap_or(0);
    let display: String = key.get_value("DisplayVersion").unwrap_or_default();
    let mut name = if product.is_empty() { "Windows".to_string() } else { product };
    if build >= 22_000 {
        name = name.replace("Windows 10", "Windows 11");
    }
    let display = display.trim();
    if !display.is_empty() {
        name.push(' ');
        name.push_str(display);
    } else if build > 0 {
        name.push_str(&format!(" (build {build})"));
    }
    name
}
