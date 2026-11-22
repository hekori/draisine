// No console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod config;
mod desktop;
mod engine;
mod gui;
mod icon;
mod ipc;
mod log;
mod rclone;
mod tray;

use engine::Mode;

const HELP: &str = "Draisine: safe rclone sync in the tray

Usage:
  draisine                         Start the tray (default)
  draisine --window [ACTION]       Open the window, optionally with a preview for ACTION
                                   ACTION: diff | pull | push | twoway
  draisine --version | --help
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("draisine {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    desktop::ensure();
    let result = if args.first().map(String::as_str) == Some("--window") {
        let action = args.get(1).and_then(|a| Mode::from_arg(a));
        gui::run(action)
    } else {
        tray::run()
    };
    if let Err(e) = result {
        log::error(format!("{e:#}"));
        std::process::exit(1);
    }
}
