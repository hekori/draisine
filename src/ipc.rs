//! Minimal file-based coordination between the tray process and the window process.
//!
//! - The window process holds `window.lock` while it is open.
//! - The tray asks the window to show itself / start an action by writing `window-request.txt`;
//!   the window polls for that file. If no window is running, the tray starts one.
//! - The tray process holds `tray.lock` (single instance).
//! - While the window compares (preview) or applies a sync it holds `window-sync.lock` (the tray
//!   shows ↻), and when it is done it writes `tray-request.txt` so the tray updates its badge.

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

/// What the window tells the tray when it is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayNote {
    /// A preview found both sides in sync: the tray can show ✓ without running anything.
    InSync,
    /// A preview found changes; the user decides in the window. The tray shows ! but must not
    /// run anything itself, or the preview would become stale.
    Pending,
    /// Files were changed, or the preview failed: the tray should re-evaluate.
    Changed,
}

/// Window side: held while a preview or sync started in the window is running.
pub fn hold_sync_lock() -> Option<File> {
    try_lock("window-sync.lock")
}

/// Tray side: whether the window is comparing or syncing right now.
pub fn window_syncing() -> bool {
    is_locked("window-sync.lock")
}

/// Window side: tells the tray the outcome of a preview or sync.
pub fn notify_tray(note: TrayNote) {
    let s = match note {
        TrayNote::InSync => "in-sync",
        TrayNote::Pending => "pending",
        TrayNote::Changed => "changed",
    };
    let _ = std::fs::write(path("tray-request.txt"), s);
}

/// Tray side: the window's latest note since the last call, if any.
pub fn take_tray_note() -> Option<TrayNote> {
    let p = path("tray-request.txt");
    let s = std::fs::read_to_string(&p).ok()?;
    let _ = std::fs::remove_file(&p);
    Some(match s.trim() {
        "in-sync" => TrayNote::InSync,
        "pending" => TrayNote::Pending,
        _ => TrayNote::Changed,
    })
}
