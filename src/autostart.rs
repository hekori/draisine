//! Start the tray when the user logs in.
//!
//! The switch lives in the OS, not in the config, so it reflects what will actually happen:
//! - Linux: `~/.config/autostart/draisine.desktop` (XDG autostart, KDE/GNOME/Xfce/…)
//! - Windows: value `Draisine` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//! - macOS: `~/Library/LaunchAgents/com.hekori.draisine.plist` with `RunAtLoad`
//!
//! The entry points at the current executable. [`refresh`] rewrites it on every start, so a
//! moved portable folder keeps starting.

use anyhow::Result;

/// Whether Draisine is registered to start at login.
pub fn is_enabled() -> bool {
    imp::is_enabled()
}

/// Turns starting at login on or off.
pub fn set(on: bool) -> Result<()> {
    let r = if on { imp::enable() } else { imp::disable() };
    match &r {
        Ok(()) => crate::log::info(format!("Start at login {}", if on { "enabled" } else { "disabled" })),
        Err(e) => crate::log::warn(format!("Start at login could not be changed: {e:#}")),
    }
    r
}

/// If enabled, points the entry at the current executable (e.g. after the folder was moved).
pub fn refresh() {
    if is_enabled() {
        if let Err(e) = imp::enable() {
            crate::log::warn(format!("Autostart entry not updated: {e:#}"));
        }
    }
}

fn exe() -> Result<std::path::PathBuf> {
    let p = std::env::current_exe()?;
    Ok(p.canonicalize().unwrap_or(p))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_if_changed(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if std::fs::read(path).is_ok_and(|old| old == bytes) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn remove(path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use anyhow::{Context, Result};
    use std::path::PathBuf;

    fn path() -> Option<PathBuf> {
        Some(directories::BaseDirs::new()?.config_dir().join("autostart/draisine.desktop"))
    }

    pub fn is_enabled() -> bool {
        // A desktop entry with Hidden=true (e.g. switched off in the desktop's own settings) is off.
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .is_some_and(|s| !s.lines().any(|l| l.trim().eq_ignore_ascii_case("hidden=true")))
    }

    pub fn enable() -> Result<()> {
        let path = path().context("no home directory")?;
        let exe = super::exe()?;
        let entry = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Draisine\n\
             Comment={}\n\
             Exec=\"{}\"\n\
             Icon=draisine\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n",
            env!("CARGO_PKG_DESCRIPTION"),
            exe.display().to_string().replace('\\', "\\\\").replace('"', "\\\""),
        );
        super::write_if_changed(&path, entry.as_bytes())
    }

    pub fn disable() -> Result<()> {
        match path() {
            Some(p) => super::remove(&p),
            None => Ok(()),
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use anyhow::{Context, Result};
    use std::path::PathBuf;

    const LABEL: &str = "com.hekori.draisine";

    fn path() -> Option<PathBuf> {
        Some(directories::BaseDirs::new()?.home_dir().join(format!("Library/LaunchAgents/{LABEL}.plist")))
    }

    pub fn is_enabled() -> bool {
        path().is_some_and(|p| p.is_file())
    }

    pub fn enable() -> Result<()> {
        let path = path().context("no home directory")?;
        let exe = super::exe()?.display().to_string().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n\
             \t<key>Label</key>\n\t<string>{LABEL}</string>\n\
             \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{exe}</string>\n\t</array>\n\
             \t<key>RunAtLoad</key>\n\t<true/>\n\
             \t<key>ProcessType</key>\n\t<string>Interactive</string>\n\
             </dict>\n</plist>\n"
        );
        super::write_if_changed(&path, plist.as_bytes())
    }

    pub fn disable() -> Result<()> {
        match path() {
            Some(p) => super::remove(&p),
            None => Ok(()),
        }
    }
}

#[cfg(windows)]
mod imp {
    use anyhow::{Result, bail};
    use std::process::Command;

    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const NAME: &str = "Draisine";

    fn reg(args: &[&str]) -> std::io::Result<std::process::Output> {
        let mut cmd = Command::new("reg");
        cmd.args(args);
        crate::rclone::no_window(&mut cmd);
        cmd.output()
    }

    fn current() -> Option<String> {
        let out = reg(&["query", KEY, "/v", NAME]).ok().filter(|o| o.status.success())?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        // "    Draisine    REG_SZ    \"C:\\…\\draisine.exe\""
        text.lines().find_map(|l| l.split_once("REG_SZ").map(|(_, v)| v.trim().to_string()))
    }

    pub fn is_enabled() -> bool {
        current().is_some()
    }

    pub fn enable() -> Result<()> {
        let value = format!("\"{}\"", super::exe()?.display());
        if current().as_deref() == Some(value.as_str()) {
            return Ok(());
        }
        let out = reg(&["add", KEY, "/v", NAME, "/t", "REG_SZ", "/d", &value, "/f"])?;
        if !out.status.success() {
            bail!("reg add: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    pub fn disable() -> Result<()> {
        if current().is_none() {
            return Ok(());
        }
        let out = reg(&["delete", KEY, "/v", NAME, "/f"])?;
        if !out.status.success() {
            bail!("reg delete: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod imp {
    use anyhow::{Result, bail};
    pub fn is_enabled() -> bool {
        false
    }
    pub fn enable() -> Result<()> {
        bail!("not supported on this system")
    }
    pub fn disable() -> Result<()> {
        Ok(())
    }
}
