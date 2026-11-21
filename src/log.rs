//! Append-only log files, one per day: <log_dir>/draisine-YYYY-MM-DD.txt (plain text, opens in any editor).
//! Logs are never deleted by the app.

use std::io::Write;
use std::path::PathBuf;

pub fn today_file() -> PathBuf {
    let day = chrono::Local::now().format("%Y-%m-%d");
    crate::config::dirs().log_dir.join(format!("draisine-{day}.txt"))
}

fn write(level: &str, msg: &str) {
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let role = if std::env::args().any(|a| a == "--window") { "ui" } else { "tray" };
    let mut text = String::new();
    for line in msg.lines() {
        text.push_str(&format!("[{ts}] {level:5} {role:4} {line}\n"));
    }
    if text.is_empty() {
        return;
    }
    eprint!("{text}");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(today_file()) {
        let _ = f.write_all(text.as_bytes());
    }
}

pub fn info(msg: impl AsRef<str>) {
    write("INFO", msg.as_ref());
}
pub fn warn(msg: impl AsRef<str>) {
    write("WARN", msg.as_ref());
}
pub fn error(msg: impl AsRef<str>) {
    write("ERROR", msg.as_ref());
}

/// Last `n` lines of today's log (for the log view).
pub fn tail(n: usize) -> String {
    let s = std::fs::read_to_string(today_file()).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Opens today's log file (.txt) with the system's text viewer.
pub fn open_today() {
    let f = today_file();
    if !f.exists() {
        info("Log angelegt");
    }
    if open::that_detached(&f).is_err() {
        let _ = open::that_detached(&crate::config::dirs().log_dir);
    }
}

pub fn open_dir() {
    let _ = open::that_detached(&crate::config::dirs().log_dir);
}
