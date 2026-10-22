//! What the machine is doing, for the About page.
//!
//! Memory, processor load and the operating system, sampled when the page asks
//! for them: there is no background thread, and nothing is read while the page
//! is shut. Each platform answers through its own module; the parsing helpers
//! below are shared by the two Unix ports and tested on every host.

use std::time::{Duration, Instant};

#[cfg(any(target_os = "linux", target_os = "android"))]
#[path = "linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;
#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos"
)))]
#[path = "other.rs"]
mod platform;

/// The machine as the About page shows it.
pub struct Vitals {
    platform: platform::Platform,
    os: String,
    total_memory: u64,
    used_memory: u64,
    cpu_percent: Option<f32>,
    sampled: Instant,
}

impl Vitals {
    /// Takes the first reading, which is also the baseline the next processor
    /// load is measured against.
    pub fn new() -> Self {
        let mut platform = platform::Platform::new();
        let (total_memory, used_memory) = platform.memory().unwrap_or((0, 0));
        // A load is measured between two readings, and this is only one; the
        // first comparison is made by the first [`Self::refresh`].
        let _ = platform.cpu();
        let os = platform.os().to_string();
        Self { platform, os, total_memory, used_memory, cpu_percent: None, sampled: Instant::now() }
    }

    /// Takes a new reading once `every` has passed since the last one, and
    /// reports whether it did.
    pub fn refresh(&mut self, every: Duration) -> bool {
        if self.sampled.elapsed() < every {
            return false;
        }
        self.sampled = Instant::now();
        if let Some((total, used)) = self.platform.memory() {
            self.total_memory = total;
            self.used_memory = used;
        }
        self.cpu_percent = self.platform.cpu();
        true
    }

    /// The operating system, as the platform names it: `Windows 11 Pro 26H2`,
    /// `Debian GNU/Linux 13 (trixie)`, `macOS 13.2.1`, `Android 14`.
    pub fn os(&self) -> &str {
        &self.os
    }

    pub fn total_memory(&self) -> u64 {
        self.total_memory
    }

    pub fn used_memory(&self) -> u64 {
        self.used_memory
    }

    /// Memory in use as a whole percentage, `None` when the platform could not
    /// answer.
    pub fn memory_percent(&self) -> Option<u32> {
        percent(self.used_memory, self.total_memory)
    }

    /// Processor load over the interval since the previous reading, `None`
    /// before the second one.
    pub fn cpu_percent(&self) -> Option<f32> {
        self.cpu_percent
    }
}

impl Default for Vitals {
    fn default() -> Self {
        Self::new()
    }
}

/// `used` as a whole percentage of `total`, rounded; `None` when there is no
/// total to divide by.
fn percent(used: u64, total: u64) -> Option<u32> {
    if total == 0 {
        return None;
    }
    Some(((used as f64 * 100.0 / total as f64).round() as u32).min(100))
}

/// The `MemTotal` and `MemAvailable` of a `/proc/meminfo`, in bytes.
///
/// Both are named in kilobytes there; a kernel too old to report `MemAvailable`
/// (before 3.14) leaves the pair `None`, and the platform module falls back to
/// the free memory instead.
#[cfg(any(target_os = "linux", target_os = "android", test))]
fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let mut total = None;
    let mut available = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Some(kib) = value.split_whitespace().next().and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        match key.trim() {
            "MemTotal" => total = Some(kib.saturating_mul(1024)),
            "MemAvailable" => available = Some(kib.saturating_mul(1024)),
            _ => {}
        }
    }
    Some((total?, available?))
}

/// The busy and total jiffies of a `/proc/stat`, in its `cpu` line.
///
/// The eight counters the line's total is made of are user, nice, system, idle,
/// iowait, irq, softirq and steal; the guest ones that can follow are already
/// counted inside user and nice, so adding them would count those twice. Idle
/// and iowait are the time the processor was not doing anything.
#[cfg(any(target_os = "linux", test))]
fn parse_proc_stat(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().find(|line| line.starts_with("cpu "))?;
    let counters: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|value| value.parse::<u64>().ok())
        .collect();
    if counters.len() < 4 {
        return None;
    }
    let total: u64 = counters.iter().take(8).sum();
    let idle = counters[3] + counters.get(4).copied().unwrap_or(0);
    Some((total.saturating_sub(idle), total))
}

/// The name a Linux distribution gives itself, from its `/etc/os-release`.
///
/// `PRETTY_NAME` is what the distribution's own tools print - "Debian GNU/Linux
/// 13 (trixie)", "Ubuntu 24.04.3 LTS" - and is used as it stands. A file without
/// one is named from its `NAME` and `VERSION_ID`.
#[cfg(any(target_os = "linux", test))]
fn parse_os_release(text: &str) -> Option<String> {
    let mut pretty = None;
    let mut name = None;
    let mut version = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_string();
        match key.trim() {
            "PRETTY_NAME" => pretty = Some(value),
            "NAME" => name = Some(value),
            "VERSION_ID" => version = Some(value),
            _ => {}
        }
    }
    pretty.or_else(|| match (name, version) {
        (Some(name), Some(version)) => Some(format!("{name} {version}")),
        (name, _) => name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_meminfo_gives_total_and_available_in_bytes() {
        let text = "MemTotal:       16384000 kB\n\
                    MemFree:         2000000 kB\n\
                    MemAvailable:    9000000 kB\n\
                    Buffers:          100000 kB\n";
        assert_eq!(parse_meminfo(text), Some((16384000 * 1024, 9000000 * 1024)));
    }

    #[test]
    fn a_meminfo_without_available_is_left_alone() {
        let text = "MemTotal:       16384000 kB\nMemFree:         2000000 kB\n";
        assert_eq!(parse_meminfo(text), None);
    }

    #[test]
    fn a_proc_stat_counts_idle_and_iowait_as_idle() {
        // user nice system idle iowait irq softirq steal
        let text = "cpu  100 0 50 1000 100 10 10 0 0 0\ncpu0 50 0 25 500 50 5 5 0 0 0\n";
        // total = 1270, idle = 1100, busy = 170
        assert_eq!(parse_proc_stat(text), Some((170, 1270)));
    }

    #[test]
    fn a_proc_stat_ignores_the_guest_counters() {
        // The trailing guest counters are already inside user and nice.
        let text = "cpu  100 0 50 1000 100 10 10 0 40 5\n";
        assert_eq!(parse_proc_stat(text), Some((170, 1270)));
    }

    #[test]
    fn an_os_release_prefers_its_pretty_name() {
        let text = "NAME=\"Debian GNU/Linux\"\n\
                    VERSION_ID=\"13\"\n\
                    PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n";
        assert_eq!(parse_os_release(text).as_deref(), Some("Debian GNU/Linux 13 (trixie)"));
    }

    #[test]
    fn an_os_release_without_a_pretty_name_is_built_from_name_and_version() {
        let text = "NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\n";
        assert_eq!(parse_os_release(text).as_deref(), Some("Ubuntu 24.04"));
    }

    #[test]
    fn an_os_release_of_comments_only_names_nothing() {
        assert_eq!(parse_os_release("# nothing here\n"), None);
    }

    #[test]
    fn a_percentage_is_rounded_and_capped() {
        assert_eq!(percent(18848 * 1024 * 1024, 32687 * 1024 * 1024), Some(58));
        assert_eq!(percent(0, 0), None);
        assert_eq!(percent(10, 10), Some(100));
        assert_eq!(percent(11, 10), Some(100));
    }

    /// The platforms the module answers on are asked for a real reading: a name
    /// for the system, and the memory installed behind it.
    #[cfg(any(
        windows,
        target_os = "linux",
        target_os = "android",
        target_os = "macos"
    ))]
    #[test]
    fn a_reading_names_the_system_and_finds_its_memory() {
        let vitals = Vitals::new();
        assert!(!vitals.os().is_empty(), "the operating system is named");
        assert!(vitals.total_memory() > 0, "{} reports no memory", vitals.os());
        assert!(vitals.total_memory() >= vitals.used_memory());
        assert!(vitals.memory_percent().is_some());
    }
}
