//! macOS: `sysctl` for the version and the amount of memory installed, and the
//! Mach host statistics for what is in use and how busy the processors are.

use std::ffi::CString;
use std::mem::size_of;
use std::ptr;

use libc::{
    c_int, c_void, host_cpu_load_info, host_statistics, host_statistics64, sysctlbyname,
    vm_statistics64, CPU_STATE_IDLE, CPU_STATE_NICE, CPU_STATE_SYSTEM, CPU_STATE_USER,
    HOST_CPU_LOAD_INFO, HOST_CPU_LOAD_INFO_COUNT, HOST_VM_INFO64, HOST_VM_INFO64_COUNT,
};

// `mach_host_self` is deprecated in the `libc` bindings in favour of a separate
// crate; the one call it saves is declared here instead, so the Mac build does
// not carry a warning for it.
extern "C" {
    fn mach_host_self() -> libc::mach_port_t;
}

pub(super) struct Platform {
    os: String,
    /// Busy and total ticks at the previous reading.
    cpu: Option<(u64, u64)>,
}

impl Platform {
    pub(super) fn new() -> Self {
        Self { os: os_name(), cpu: cpu_ticks() }
    }

    pub(super) fn os(&self) -> &str {
        &self.os
    }

    /// Total and used physical memory, both in bytes.
    ///
    /// "Used" is what Activity Monitor calls Memory Used - pages that are
    /// active, wired, or held compressed - so the file cache does not count as
    /// held, the way `MemAvailable` keeps it out of Linux's figure and
    /// `ullAvailPhys` out of Windows's.
    pub(super) fn memory(&self) -> Option<(u64, u64)> {
        let total = sysctl_u64("hw.memsize")?;
        let page = sysctl_u64("hw.pagesize").unwrap_or(4096);
        let mut stats: vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = HOST_VM_INFO64_COUNT;
        // SAFETY: `stats` is a `vm_statistics64` and `count` its length in words,
        // which is the shape `host_statistics64` writes for `HOST_VM_INFO64`.
        let result = unsafe {
            host_statistics64(
                mach_host_self(),
                HOST_VM_INFO64,
                &mut stats as *mut vm_statistics64 as *mut c_int,
                &mut count,
            )
        };
        if result != 0 {
            return None;
        }
        let pages = u64::from(stats.active_count)
            + u64::from(stats.wire_count)
            + u64::from(stats.compressor_page_count);
        Some((total, (pages * page).min(total)))
    }

    pub(super) fn cpu(&mut self) -> Option<f32> {
        let now = cpu_ticks()?;
        let previous = self.cpu.replace(now)?;
        let busy = now.0.saturating_sub(previous.0);
        let total = now.1.saturating_sub(previous.1);
        if total == 0 {
            return None;
        }
        Some(busy as f32 * 100.0 / total as f32)
    }
}

/// Busy and total ticks of every processor, summed.
fn cpu_ticks() -> Option<(u64, u64)> {
    let mut info: host_cpu_load_info = unsafe { std::mem::zeroed() };
    let mut count = HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: `info` is a `host_cpu_load_info` and `count` its length in words,
    // which is the shape `host_statistics` writes for `HOST_CPU_LOAD_INFO`.
    let result = unsafe {
        host_statistics(
            mach_host_self(),
            HOST_CPU_LOAD_INFO,
            &mut info as *mut host_cpu_load_info as *mut c_int,
            &mut count,
        )
    };
    if result != 0 {
        return None;
    }
    let ticks = info.cpu_ticks;
    let user = u64::from(ticks[CPU_STATE_USER as usize]);
    let system = u64::from(ticks[CPU_STATE_SYSTEM as usize]);
    let idle = u64::from(ticks[CPU_STATE_IDLE as usize]);
    let nice = u64::from(ticks[CPU_STATE_NICE as usize]);
    let busy = user + system + nice;
    Some((busy, busy + idle))
}

/// "macOS 13.2.1", from the product version the system reports.
fn os_name() -> String {
    match sysctl_string("kern.osproductversion") {
        Some(version) => format!("macOS {version}"),
        None => "macOS".to_string(),
    }
}

/// A `sysctl` value as a 64-bit integer.
fn sysctl_u64(name: &str) -> Option<u64> {
    let name = CString::new(name).ok()?;
    let mut value: u64 = 0;
    let mut size = size_of::<u64>();
    // SAFETY: the name is NUL-terminated, and the buffer is as long as the
    // length handed over. `hw.memsize` is 64-bit and `hw.pagesize` 32-bit, which
    // writes into the low half of the zeroed value.
    let result = unsafe {
        sysctlbyname(
            name.as_ptr(),
            &mut value as *mut u64 as *mut c_void,
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    (result == 0).then_some(value)
}

/// A `sysctl` string value.
fn sysctl_string(name: &str) -> Option<String> {
    let name = CString::new(name).ok()?;
    let mut buffer = [0u8; 64];
    let mut size = buffer.len();
    // SAFETY: the name is NUL-terminated, and the buffer is as long as the length
    // handed over; the kernel writes at most that many bytes.
    let result = unsafe {
        sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr() as *mut c_void,
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return None;
    }
    let end = buffer.iter().position(|&byte| byte == 0).unwrap_or(size);
    String::from_utf8(buffer[..end].to_vec()).ok()
}
