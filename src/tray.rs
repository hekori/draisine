//! Tray process: tray icon + menu, and the auto-sync worker.
//!
//! Auto-sync runs a two-way sync for each pair that has auto-sync on and has been synced manually at
//! least once (see `engine::auto_sync_active`), every N minutes as set per pair, unless it is paused
//! from the tray menu. It only executes actions that
//! cannot lose data (new files, conflicts ➡ conflict files). Deletes and overwrites wait for the
//! user's confirmation in the window; the tray badge turns orange and a notification is shown.

use crate::config::{APP_NAME, Config};
use crate::engine::{self, Mode};
use crate::icon::{self, Status};
use crate::{ipc, log};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowId;

#[derive(Debug)]
enum UserEvent {
    /// With the XDG activation token of the click, if the tray host sent one.
    Menu(MenuEvent, Option<String>),
    Tray(TrayIconEvent, Option<String>),
    Refresh,
}

enum WorkerCmd {
    RunNow,
    Quit,
}

#[derive(Clone)]
struct Shared {
    status: Status,
    text: String,
    paused: bool,
}

struct Items {
    status: MenuItem,
    open: MenuItem,
    diff: MenuItem,
    pull: MenuItem,
    push: MenuItem,
    twoway: MenuItem,
    pause: CheckMenuItem,
    run_now: MenuItem,
    logs: MenuItem,
    log_dir: MenuItem,
    quit: MenuItem,
}

struct App {
    tray: Option<TrayIcon>,
    items: Items,
    menu: Menu,
    shared: Arc<Mutex<Shared>>,
    worker: Sender<WorkerCmd>,
}

fn tray_icon(status: Status) -> tray_icon::Icon {
    // 44 = 2 x 22: pixel-exact on HiDPI panels, and an exact halving for the usual 22 px slot.
    tray_icon::Icon::from_rgba(icon::tray_rgba(44, status), 44, 44).expect("icon")
}

impl App {
    fn refresh(&mut self) {
        let sh = self.shared.lock().unwrap().clone();
        self.items.status.set_text(&sh.text);
        self.items.pause.set_checked(sh.paused);
        if let Some(t) = &self.tray {
            let _ = t.set_icon(Some(tray_icon(sh.status)));
            let _ = t.set_tooltip(Some(format!("{APP_NAME}: {}", sh.text)));
        }
    }

    fn handle_menu(&mut self, ev: MenuEvent, token: Option<String>, el: &ActiveEventLoop) {
        let id = ev.id;
        let i = &self.items;
        if id == *i.open.id() {
            ipc::request_window(None, token);
        } else if id == *i.diff.id() {
            ipc::request_window(Some(Mode::Diff), token);
        } else if id == *i.pull.id() {
            ipc::request_window(Some(Mode::Pull), token);
        } else if id == *i.push.id() {
            ipc::request_window(Some(Mode::Push), token);
        } else if id == *i.twoway.id() {
            ipc::request_window(Some(Mode::TwoWay), token);
        } else if id == *i.pause.id() {
            let mut cfg = Config::load();
            cfg.auto_sync_paused = i.pause.is_checked();
            log::info(format!("Auto-sync {}", if cfg.auto_sync_paused { "paused" } else { "resumed" }));
            if let Err(e) = cfg.save() {
                log::error(format!("Saving configuration: {e}"));
            }
        } else if id == *i.run_now.id() {
            let _ = self.worker.send(WorkerCmd::RunNow);
        } else if id == *i.logs.id() {
            log::open_today();
        } else if id == *i.log_dir.id() {
            log::open_dir();
        } else if id == *i.quit.id() {
            log::info("Draisine stopped");
            let _ = self.worker.send(WorkerCmd::Quit);
            self.tray = None;
            el.exit();
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn new_events(&mut self, _el: &ActiveEventLoop, cause: StartCause) {
        // The tray icon must be created once the event loop is running (macOS requirement).
        if cause == StartCause::Init && self.tray.is_none() {
            let status = self.shared.lock().unwrap().status;
            match TrayIconBuilder::new()
                .with_id("draisine")
                .with_title(APP_NAME)
                .with_tooltip(APP_NAME)
                .with_icon(tray_icon(status))
                .with_menu(Box::new(self.menu.clone()))
                .with_menu_on_left_click(false)
                .build()
            {
                Ok(t) => self.tray = Some(t),
                Err(e) => log::error(format!("Could not create the tray icon: {e}")),
            }
            self.refresh();
        }
    }

    fn resumed(&mut self, _el: &ActiveEventLoop) {}

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, _ev: WindowEvent) {}

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Menu(m, token) => self.handle_menu(m, token, el),
            UserEvent::Tray(
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                | TrayIconEvent::DoubleClick { .. },
                token,
            ) => ipc::toggle_window(token),
            UserEvent::Tray(..) => {}
            UserEvent::Refresh => self.refresh(),
        }
    }
}

fn set(shared: &Arc<Mutex<Shared>>, proxy: &EventLoopProxy<UserEvent>, status: Status, text: String) {
    {
        let mut s = shared.lock().unwrap();
        s.status = status;
        s.text = text;
    }
    let _ = proxy.send_event(UserEvent::Refresh);
}

fn notify(body: &str) {
    let _ = notify_rust::Notification::new().summary(APP_NAME).body(body).appname(APP_NAME).show();
}

/// Ids of the pairs the background auto-sync includes right now.
fn auto_pairs(cfg: &Config) -> Vec<String> {
    cfg.pairs.iter().filter(|p| engine::auto_sync_active(p)).map(|p| p.id.clone()).collect()
}

fn idle_status(cfg: &Config) -> (Status, String) {
    let n = auto_pairs(cfg).len();
    if cfg.pairs.is_empty() {
        (Status::Off, "no folder pairs set up".into())
    } else if n == 0 {
        (Status::Paused, "Auto-sync off".into())
    } else if cfg.auto_sync_paused {
        (Status::Paused, "Auto-sync paused".into())
    } else {
        let what = if n == 1 { "1 folder pair".to_string() } else { format!("{n} folder pairs") };
        (Status::Idle, format!("Auto-sync on for {what}"))
    }
}

/// Outcome states that stay visible until the next run, even when the configuration changes.
fn sticky(s: Status) -> bool {
    matches!(s, Status::Attention | Status::Error | Status::AuthExpired | Status::Offline)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorKind {
    Offline,
    Auth,
    Other,
}

/// Sorts an error by the rclone / OS message it carries.
fn classify(msg: &str) -> ErrorKind {
    const AUTH: &[&str] = &[
        "invalid_grant",
        "cannot fetch token",
        "couldn't fetch token",
        "failed to get token",
        "token expired",
        "expired_access_token",
        "invalid_access_token",
        "token has been expired or revoked",
        "unauthorized",
        "re-authenticate",
        "reconnect",
    ];
    const OFFLINE: &[&str] = &[
        "no such host",
        "temporary failure in name resolution",
        "network is unreachable",
        "no route to host",
        "dial tcp",
        "i/o timeout",
        "tls handshake timeout",
        "connection refused",
        "connection reset",
        "context deadline exceeded",
        "server misbehaving",
        "no internet",
    ];
    let m = msg.to_lowercase();
    if AUTH.iter().any(|k| m.contains(k)) {
        ErrorKind::Auth
    } else if OFFLINE.iter().any(|k| m.contains(k)) {
        ErrorKind::Offline
    } else {
        ErrorKind::Other
    }
}

fn worker(rx: mpsc::Receiver<WorkerCmd>, shared: Arc<Mutex<Shared>>, proxy: EventLoopProxy<UserEvent>) {
    let mut cfg = Config::load();
    let mut auto_ids = auto_pairs(&cfg);
    let mut mtime = Config::mtime();
    // Per pair: when auto-sync last ran it. Missing: due right away.
    let mut last_run: HashMap<String, Instant> = HashMap::new();
    let mut last_note = String::new();
    let mut window_busy = false;
    let mut before_window: Option<(Status, String)> = None;
    loop {
        let mut force = false;
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(WorkerCmd::RunNow) => force = true,
            Ok(WorkerCmd::Quit) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        // A preview or sync started in the window: show ↻ while it runs, then take over its outcome.
        let busy_now = ipc::window_syncing();
        if busy_now && !window_busy {
            before_window = Some({
                let s = shared.lock().unwrap();
                (s.status, s.text.clone())
            });
            set(&shared, &proxy, Status::Busy, "Comparing / syncing in the window …".into());
        }
        let note = ipc::take_tray_note();
        let released = window_busy && !busy_now;
        window_busy = busy_now;
        if busy_now {
            continue; // the engine's lock would refuse an auto-sync now anyway
        }
        match note {
            Some(ipc::TrayNote::InSync) => {
                // The window just compared everything: nothing to do, nothing waiting.
                let now = chrono::Local::now().format("%H:%M");
                last_note.clear();
                let (st, t) = idle_status(&cfg);
                let t = if st == Status::Idle { format!("{t}, in sync ({now})") } else { t };
                set(&shared, &proxy, st, t);
            }
            Some(ipc::TrayNote::Pending) => {
                let now = chrono::Local::now().format("%H:%M");
                set(&shared, &proxy, Status::Attention, format!("Changes found in the preview ({now})"));
            }
            Some(ipc::TrayNote::Changed) if !auto_ids.is_empty() && !cfg.auto_sync_paused => {
                // A safe two-way run computes the real state: ✓, ! (still waiting), offline, …
                force = true;
            }
            Some(ipc::TrayNote::Changed) => {
                // Auto-sync off: don't touch files, just drop the old outcome badge.
                last_note.clear();
                let (st, t) = idle_status(&cfg);
                set(&shared, &proxy, st, t);
            }
            None if released => {
                // Cancelled, or the window went away: back to what was shown before.
                if let Some((st, t)) = before_window.take() {
                    set(&shared, &proxy, st, t);
                }
            }
            None => {}
        }
        // A manual sync also changes which pairs may auto-sync (the first one makes them eligible).
        let m = Config::mtime();
        let cfg_changed = m != mtime;
        if cfg_changed || note.is_some() {
            if cfg_changed {
                mtime = m;
                let new = Config::load();
                if cfg.auto_sync_paused && !new.auto_sync_paused {
                    last_run.clear(); // resuming runs right away
                }
                cfg = new;
                shared.lock().unwrap().paused = cfg.auto_sync_paused;
            }
            let ids = auto_pairs(&cfg);
            last_run.retain(|id, _| ids.contains(id)); // turning auto-sync on for a pair runs it right away
            auto_ids = ids;
        }
        if cfg_changed {
            let s = shared.lock().unwrap().status;
            if !sticky(s) {
                let (st, t) = idle_status(&cfg);
                set(&shared, &proxy, st, t);
            } else {
                let _ = proxy.send_event(UserEvent::Refresh);
            }
        }
        // “Run auto-sync now” runs every auto-sync pair, even while paused.
        let pairs: Vec<_> = cfg
            .pairs
            .iter()
            .filter(|p| auto_ids.contains(&p.id))
            .filter(|p| {
                let interval = Duration::from_secs(p.interval_minutes.max(1) as u64 * 60);
                force || (!cfg.auto_sync_paused && last_run.get(&p.id).is_none_or(|t| t.elapsed() >= interval))
            })
            .cloned()
            .collect();
        if pairs.is_empty() {
            if force {
                let (st, t) = idle_status(&cfg);
                set(&shared, &proxy, st, t);
            }
            continue;
        }
        for p in &pairs {
            last_run.insert(p.id.clone(), Instant::now());
        }
        set(&shared, &proxy, Status::Busy, "Auto-sync running …".into());
        log::info("Auto-sync started");
        let (mut pending, mut conflicts, mut done, mut errors) = (0, 0, 0, Vec::new());
        for p in &pairs {
            match engine::auto_sync(&cfg, p) {
                Ok((r, n)) => {
                    pending += n;
                    conflicts += r.conflicts.len();
                    done += r.done;
                    errors.extend(r.failed.iter().map(|f| format!("{}: {f}", p.label())));
                }
                Err(e) => {
                    log::error(format!("[{}] Auto-Sync: {e:#}", p.label()));
                    errors.push(format!("{}: {e:#}", p.label()));
                }
            }
        }
        let now = chrono::Local::now().format("%H:%M");
        let kinds: Vec<_> = errors.iter().map(|e| classify(e)).collect();
        let (status, text, note) = if kinds.contains(&ErrorKind::Other) {
            let e = errors.iter().zip(&kinds).find(|(_, k)| **k == ErrorKind::Other).unwrap().0;
            (Status::Error, format!("Auto-sync error ({now}), see log"), format!("Sync error:\n{e}"))
        } else if let Some(i) = kinds.iter().position(|k| *k == ErrorKind::Auth) {
            let pair = errors[i].split(':').next().unwrap_or_default();
            (
                Status::AuthExpired,
                format!("Sign-in expired ({now}), please reconnect"),
                format!("The sign-in for {pair} has expired. Please reconnect it in Draisine."),
            )
        } else if !errors.is_empty() {
            // Offline: expected on laptops, so no notification, just the badge.
            (Status::Offline, format!("Offline, last attempt {now}"), String::new())
        } else if pending > 0 {
            (
                Status::Attention,
                format!("{pending} changes waiting for confirmation ({now})"),
                format!("{pending} files are to be deleted or overwritten. Please confirm via “Two-way sync …” in the menu."),
            )
        } else {
            let (_, t) = idle_status(&cfg);
            (Status::Idle, format!("{t}, last run {now}: {done} actions"), String::new())
        };
        let note = if conflicts > 0 {
            format!("{conflicts} conflict(s): both versions were kept as conflict files.\n{note}")
        } else {
            note
        };
        log::info(format!("Auto-sync done: {done} actions, {conflicts} conflicts, {pending} waiting, {} errors", errors.len()));
        if !note.is_empty() && note != last_note {
            notify(&note);
        }
        last_note = note;
        set(&shared, &proxy, status, text);
    }
}

/// The XDG activation token of the tray event being handled. Only valid inside the tray-icon
/// event handlers, which ksni calls while it handles the host's D-Bus call.
fn activation_token() -> Option<String> {
    #[cfg(target_os = "linux")]
    return ksni::current_xdg_activation_token();
    #[cfg(not(target_os = "linux"))]
    None
}

pub fn run() -> Result<()> {
    let Some(_tray_lock) = ipc::try_lock("tray.lock") else {
        // Already running: just open the window.
        ipc::request_window(None, ipc::env_token());
        return Ok(());
    };
    log::info(format!("Draisine {} started (data: {})", env!("CARGO_PKG_VERSION"), crate::config::dirs().state_dir.display()));

    #[allow(unused_mut)]
    let mut builder = EventLoop::<UserEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory);
    }
    let event_loop = builder.build()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Tray(e, activation_token()));
    }));
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Menu(e, activation_token()));
    }));

    let cfg = Config::load();
    let (st, text) = idle_status(&cfg);
    let shared = Arc::new(Mutex::new(Shared { status: st, text, paused: cfg.auto_sync_paused }));

    let items = Items {
        status: MenuItem::new("", false, None),
        open: MenuItem::new("Maximize window", true, None),
        diff: MenuItem::new("Diff local ↔ remote …", true, None),
        pull: MenuItem::new("One-way sync remote ➡ local …", true, None),
        push: MenuItem::new("One-way sync local ➡ remote …", true, None),
        twoway: MenuItem::new("Two-way sync local ↔ remote …", true, None),
        pause: CheckMenuItem::new("Pause auto-sync", true, cfg.auto_sync_paused, None),
        run_now: MenuItem::new("Run auto-sync now", true, None),
        logs: MenuItem::new("Open log (TXT)", true, None),
        log_dir: MenuItem::new("Open log folder", true, None),
        quit: MenuItem::new("Quit", true, None),
    };
    let menu = Menu::new();
    let sep = PredefinedMenuItem::separator;
    menu.append_items(&[
        &items.status,
        &sep(),
        &items.open,
        &sep(),
        &items.diff,
        &items.pull,
        &items.push,
        &items.twoway,
        &sep(),
        &items.pause,
        &items.run_now,
        &sep(),
        &items.logs,
        &items.log_dir,
        &sep(),
        &items.quit,
    ])?;

    let (tx, rx) = mpsc::channel();
    {
        let shared = shared.clone();
        let proxy = event_loop.create_proxy();
        std::thread::spawn(move || worker(rx, shared, proxy));
    }

    if cfg.pairs.is_empty() {
        // First start: open the setup assistant right away.
        ipc::request_window(None, ipc::env_token());
    }
    let mut app = App { tray: None, items, menu, shared, worker: tx };
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_errors() {
        use ErrorKind::*;
        let auth = "gdrive: Listing gdrive: failed:\nCRITICAL: Failed to create file system: couldn't fetch token: invalid_grant: maybe token expired? - try refreshing with \"rclone config reconnect gdrive:\"";
        assert_eq!(classify(auth), Auth);
        let net = "onedrive: Listing onedrive: failed:\nERROR: Get \"https://graph.microsoft.com/v1.0/me/drive\": dial tcp: lookup graph.microsoft.com: no such host";
        assert_eq!(classify(net), Offline);
        assert_eq!(classify("Photos: Another sync is running. Please try again later."), Other);
        assert_eq!(classify("Docs: 401 files compared"), Other);
    }
}
