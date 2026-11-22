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

/// A request from the tray (or a second `draisine --window`) to the window.
pub struct WindowRequest {
    /// None: just show the window.
    pub action: Option<Mode>,
    /// Tray icon click: minimize the window if it is in front, otherwise show it.
    pub toggle: bool,
    /// Show the two-way preview of the pairs with changes waiting for confirmation.
    pub review: bool,
    /// XDG activation token of the click that caused the request. On Wayland a window may only
    /// raise itself with such a token.
    pub token: Option<String>,
}

/// The XDG activation token this process was started with (e.g. by the app launcher), if any.
pub fn env_token() -> Option<String> {
    std::env::var("XDG_ACTIVATION_TOKEN").ok().filter(|t| !t.is_empty())
}

pub fn request_window(action: Option<Mode>, token: Option<String>) {
    send_request(action.map(|m| m.arg()).unwrap_or("show"), token);
}

/// Shows the window with the preview of everything auto-sync held back.
pub fn request_review(token: Option<String>) {
    send_request("review", token);
}

/// Tray icon click: shows the window, or minimizes it when it is in front.
pub fn toggle_window(token: Option<String>) {
    send_request("toggle", token);
}

fn send_request(kind: &str, token: Option<String>) {
    let mut req = kind.to_string();
    if let Some(t) = &token {
        req.push('\n');
        req.push_str(t);
    }
    let _ = std::fs::write(path("window-request.txt"), req);
    if !is_locked("window.lock") {
        match std::env::current_exe() {
            Ok(exe) => {
                let mut cmd = std::process::Command::new(exe);
                cmd.arg("--window");
                // A new window picks the token up from the environment and gets the focus.
                match &token {
                    Some(t) => cmd.env("XDG_ACTIVATION_TOKEN", t),
                    None => cmd.env_remove("XDG_ACTIVATION_TOKEN"),
                };
                crate::rclone::no_window(&mut cmd);
                if let Err(e) = cmd.spawn() {
                    crate::log::error(format!("Starting the window failed: {e}"));
                }
            }
            Err(e) => crate::log::error(format!("Program path unknown: {e}")),
        }
    }
}

/// Window side: the pending request, if any.
pub fn take_request() -> Option<WindowRequest> {
    let p = path("window-request.txt");
    let s = std::fs::read_to_string(&p).ok()?;
    let _ = std::fs::remove_file(&p);
    let mut lines = s.lines();
    let kind = lines.next().unwrap_or("").trim();
    let token = lines.next().map(str::trim).filter(|t| !t.is_empty()).map(String::from);
    Some(WindowRequest { action: Mode::from_arg(kind), toggle: kind == "toggle", review: kind == "review", token })
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
