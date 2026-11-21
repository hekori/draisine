//! Minimal file-based coordination between the tray process and the window process.
//!
//! - The window process holds `window.lock` while it is open.
//! - The tray asks the window to show itself / start an action by writing `window-request.txt`;
//!   the window polls for that file. If no window is running, the tray starts one.
//! - The tray process holds `tray.lock` (single instance).

use crate::config::dirs;
use crate::engine::Mode;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;

fn path(name: &str) -> PathBuf {
    dirs().runtime_dir.join(name)
}

/// Tries to take an exclusive lock that is held for the lifetime of the returned file.
pub fn try_lock(name: &str) -> Option<File> {
    let f = OpenOptions::new().create(true).truncate(false).write(true).open(path(name)).ok()?;
    f.try_lock().ok().map(|_| f)
}

fn is_locked(name: &str) -> bool {
    try_lock(name).is_none()
}

pub fn request_window(action: Option<Mode>) {
    let req = action.map(|m| m.arg()).unwrap_or("show");
    let _ = std::fs::write(path("window-request.txt"), req);
    if !is_locked("window.lock") {
        match std::env::current_exe() {
            Ok(exe) => {
                let mut cmd = std::process::Command::new(exe);
                cmd.arg("--window");
                crate::rclone::no_window(&mut cmd);
                if let Err(e) = cmd.spawn() {
                    crate::log::error(format!("Starting the window failed: {e}"));
                }
            }
            Err(e) => crate::log::error(format!("Program path unknown: {e}")),
        }
    }
}

/// Window side: returns Some(None) for "show", Some(Some(mode)) for an action request.
pub fn take_request() -> Option<Option<Mode>> {
    let p = path("window-request.txt");
    let s = std::fs::read_to_string(&p).ok()?;
    let _ = std::fs::remove_file(&p);
    Some(Mode::from_arg(s.trim()))
}
