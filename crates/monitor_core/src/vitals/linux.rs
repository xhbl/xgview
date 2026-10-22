//! Linux and Android: `/proc` for memory, processor load on Linux, and each
//! platform's own idea of what it is called.

use super::parse_meminfo;
#[cfg(not(target_os = "android"))]
use super::{parse_os_release, parse_proc_stat};

pub(super) struct Platform {
    os: String,
    /// Busy and total jiffies at the previous reading.
    ///
    /// Linux only: Android does not let an application read the processor
    /// counters, so the load is not offered there at all.
    #[cfg(not(target_os = "android"))]
    cpu: Option<(u64, u64)>,
}

impl Platform {
    pub(super) fn new() -> Self {
        #[cfg(not(target_os = "android"))]
        {
            Self { os: os_name(), cpu: read_cpu() }
        }
        #[cfg(target_os = "android")]
        {
            Self { os: os_name() }
        }
    }

    pub(super) fn os(&self) -> &str {
        &self.os
    }

    /// Total and used physical memory, both in bytes.
    ///
    /// "Used" is what is not available to a process - `MemTotal` less
    /// `MemAvailable` - which is what `free` reports as used, and counts the
    /// page cache as available rather than held.
    pub(super) fn memory(&self) -> Option<(u64, u64)> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let (total, available) = parse_meminfo(&text)?;
        Some((total, total.saturating_sub(available)))
    }

    /// The processor load since the previous reading.
    ///
    /// Android has none: its `/proc/stat` is not readable by an application,
    /// so the About page leaves the line out there.
    #[cfg(target_os = "android")]
    pub(super) fn cpu(&mut self) -> Option<f32> {
        None
    }

    #[cfg(not(target_os = "android"))]
    pub(super) fn cpu(&mut self) -> Option<f32> {
        let now = read_cpu()?;
        let previous = self.cpu.replace(now)?;
        let busy = now.0.saturating_sub(previous.0);
        let total = now.1.saturating_sub(previous.1);
        if total == 0 {
            return None;
        }
        Some(busy as f32 * 100.0 / total as f32)
    }
}

#[cfg(not(target_os = "android"))]
fn read_cpu() -> Option<(u64, u64)> {
    parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
}

/// "Android 14": the release the device reports, from its system properties.
#[cfg(target_os = "android")]
fn os_name() -> String {
    android_property("ro.build.version.release")
        .map(|release| format!("Android {release}"))
        .unwrap_or_else(|| "Android".to_string())
}

/// A system property, as `__system_property_get` reads one.
#[cfg(target_os = "android")]
fn android_property(name: &str) -> Option<String> {
    use std::ffi::CString;

    let name = CString::new(name).ok()?;
    // `PROP_VALUE_MAX` is 92, terminator included.
    let mut buffer = [0 as libc::c_char; 92];
    // SAFETY: the name is a NUL-terminated string, and the buffer is `PROP_VALUE_MAX`
    // bytes long, which is the most the property service writes.
    let length = unsafe { libc::__system_property_get(name.as_ptr(), buffer.as_mut_ptr()) };
    if length <= 0 {
        return None;
    }
    let bytes: Vec<u8> = buffer[..length as usize].iter().map(|&byte| byte as u8).collect();
    String::from_utf8(bytes).ok()
}

/// The distribution's own name, from `/etc/os-release`.
#[cfg(not(target_os = "android"))]
fn os_name() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| parse_os_release(&text))
        .unwrap_or_else(|| "Linux".to_string())
}
