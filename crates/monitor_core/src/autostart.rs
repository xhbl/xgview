//! Cross platform start-on-boot helpers.
//!
//! | Platform      | Mechanism                                              |
//! |---------------|--------------------------------------------------------|
//! | Windows       | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`   |
//! | Linux / macOS | `~/.config/autostart/xgview.desktop` (XDG autostart)   |
//! | Android       | `RECEIVE_BOOT_COMPLETED` + `BootReceiver` (manifest)   |
//!
//! On Windows the per-user `Run` key is used because it does not require
//! elevation. Deployments that must start before any user logs in should use
//! the Task Scheduler script shipped in `scripts/install-windows.ps1`.

use std::path::PathBuf;

use crate::error::{CoreError, Result};
// Only the Windows registry and the XDG desktop entry below use the display
// name; on Android there is neither, and the import would be unused.
#[cfg(any(windows, all(unix, not(target_os = "android"))))]
use crate::APP_DISPLAY_NAME;

/// Result of a start-on-boot query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutostartStatus {
    /// `false` when the platform has no supported mechanism.
    pub supported: bool,
    /// Whether XGView is currently registered for auto start.
    pub enabled: bool,
    /// Human readable mechanism name, e.g. `Registry (HKCU Run)`.
    pub mechanism: String,
    /// Extra information shown in the settings panel.
    pub detail: String,
}

/// Absolute path of the running executable.
pub fn current_exe() -> Result<PathBuf> {
    std::env::current_exe()
        .map_err(|err| CoreError::config(format!("cannot resolve executable path: {err}")))
}

/// Command line registered for auto start (quoted for the Windows registry).
pub fn command_line() -> Result<String> {
    let exe = current_exe()?;
    Ok(format!("\"{}\" --autostart", exe.display()))
}

/// `true` when this platform supports programmatic registration.
pub fn is_supported() -> bool {
    cfg!(any(windows, unix))
}

/// Mechanism used on this platform.
pub fn mechanism() -> &'static str {
    #[cfg(windows)]
    {
        "autostart-mechanism-windows"
    }
    #[cfg(all(unix, not(target_os = "android")))]
    {
        "autostart-mechanism-xdg"
    }
    #[cfg(target_os = "android")]
    {
        "autostart-mechanism-android"
    }
    #[cfg(not(any(windows, unix)))]
    {
        "autostart-mechanism-unsupported"
    }
}

/// Reads the current registration state.
pub fn is_enabled() -> Result<bool> {
    imp::is_enabled()
}

/// Registers or removes XGView from the start-up list.
pub fn set_enabled(enable: bool) -> Result<()> {
    imp::set_enabled(enable)
}

/// Convenience wrapper returning everything the settings panel needs.
pub fn status() -> AutostartStatus {
    let supported = is_supported();
    let enabled = if supported { is_enabled().unwrap_or(false) } else { false };
    AutostartStatus {
        supported,
        enabled,
        mechanism: mechanism().to_string(),
        detail: imp::detail(),
    }
}

#[cfg(windows)]
mod imp {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
    use winreg::RegKey;

    use super::*;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    pub(super) fn is_enabled() -> Result<bool> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = match hkcu.open_subkey_with_flags(RUN_KEY, KEY_READ) {
            Ok(key) => key,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err.into()),
        };
        Ok(key.get_value::<String, _>(APP_DISPLAY_NAME).is_ok())
    }

    pub(super) fn set_enabled(enable: bool) -> Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if enable {
            let (key, _) = hkcu.create_subkey(RUN_KEY)?;
            let command = command_line()?;
            key.set_value(APP_DISPLAY_NAME, &command)?;
            tracing::info!(target: "xgview::autostart", command = %command, "registered for start-on-boot");
        } else {
            let key = match hkcu.open_subkey_with_flags(RUN_KEY, KEY_WRITE) {
                Ok(key) => key,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(err) => return Err(err.into()),
            };
            match key.delete_value(APP_DISPLAY_NAME) {
                Ok(()) => tracing::info!(target: "xgview::autostart", "start-on-boot registration removed"),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    pub(super) fn detail() -> String {
        format!(r"HKCU\{RUN_KEY} -> {APP_DISPLAY_NAME}")
    }
}

#[cfg(all(unix, not(target_os = "android")))]
mod imp {
    use std::fs;

    use super::*;

    pub(super) fn desktop_file() -> Option<PathBuf> {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            return Some(
                PathBuf::from(xdg).join("autostart").join(format!("{}.desktop", crate::APP_NAME)),
            );
        }
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join(".config/autostart")
                .join(format!("{}.desktop", crate::APP_NAME))
        })
    }

    fn desktop_entry() -> Result<String> {
        let command = command_line()?;
        Ok(format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={APP_DISPLAY_NAME}\n\
             Comment=A grid viewer for surveillance cameras\n\
             Exec={command}\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n\
             Categories=AudioVideo;Player;\n"
        ))
    }

    pub(super) fn is_enabled() -> Result<bool> {
        Ok(desktop_file().map(|path| path.exists()).unwrap_or(false))
    }

    pub(super) fn set_enabled(enable: bool) -> Result<()> {
        let path = desktop_file()
            .ok_or_else(|| CoreError::unsupported("no HOME / XDG_CONFIG_HOME available"))?;
        if enable {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, desktop_entry()?)?;
            tracing::info!(target: "xgview::autostart", path = %path.display(), "autostart entry written");
        } else if path.exists() {
            fs::remove_file(&path)?;
            tracing::info!(target: "xgview::autostart", path = %path.display(), "autostart entry removed");
        }
        Ok(())
    }

    pub(super) fn detail() -> String {
        desktop_file()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "~/.config/autostart".to_string())
    }
}

#[cfg(target_os = "android")]
mod imp {
    use super::*;

    /// On Android the boot receiver is declared in the manifest and cannot be
    /// toggled at runtime, so the application always reports it as registered.
    pub(super) fn is_enabled() -> Result<bool> {
        Ok(true)
    }

    pub(super) fn set_enabled(enable: bool) -> Result<()> {
        tracing::info!(
            target: "xgview::autostart",
            enable,
            "Android start-on-boot is controlled by the manifest receiver"
        );
        Ok(())
    }

    pub(super) fn detail() -> String {
        "com.xhbl.xgview.BootReceiver".to_string()
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    use super::*;

    pub(super) fn is_enabled() -> Result<bool> {
        Ok(false)
    }

    pub(super) fn set_enabled(_enable: bool) -> Result<()> {
        Err(CoreError::unsupported("start-on-boot is not available on this platform"))
    }

    pub(super) fn detail() -> String {
        "unsupported platform".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_is_quoted() {
        let command = command_line().unwrap();
        assert!(command.starts_with('"'));
        assert!(command.contains("--autostart"));
    }

    #[test]
    fn status_is_available() {
        let status = status();
        assert_eq!(status.supported, is_supported());
        assert!(!status.mechanism.is_empty());
    }
}
