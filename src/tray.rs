//! Tray process: tray icon + menu, and the auto-sync worker.
//!
//! Auto-sync runs a two-way sync per enabled pair every N minutes but only executes actions that
//! cannot lose data (new files, conflicts ➡ conflict files). Deletes and overwrites wait for the
//! user's confirmation in the window; the tray turns orange and a notification is shown.

use crate::config::{APP_NAME, Config};
use crate::engine::{self, Mode};
use crate::icon::{self, Status};
use crate::{ipc, log};
use anyhow::Result;
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
    Menu(MenuEvent),
    Tray(TrayIconEvent),
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
    auto: bool,
}

struct Items {
    status: MenuItem,
    open: MenuItem,
    diff: MenuItem,
    pull: MenuItem,
    push: MenuItem,
    twoway: MenuItem,
    auto: CheckMenuItem,
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
    tray_icon::Icon::from_rgba(icon::rgba(64, status.color()), 64, 64).expect("icon")
}

impl App {
    fn refresh(&mut self) {
        let sh = self.shared.lock().unwrap().clone();
        self.items.status.set_text(&sh.text);
        self.items.auto.set_checked(sh.auto);
        if let Some(t) = &self.tray {
            let _ = t.set_icon(Some(tray_icon(sh.status)));
            let _ = t.set_tooltip(Some(format!("{APP_NAME}: {}", sh.text)));
        }
    }

    fn handle_menu(&mut self, ev: MenuEvent, el: &ActiveEventLoop) {
        let id = ev.id;
        let i = &self.items;
        if id == *i.open.id() {
            ipc::request_window(None);
        } else if id == *i.diff.id() {
            ipc::request_window(Some(Mode::Diff));
        } else if id == *i.pull.id() {
            ipc::request_window(Some(Mode::Pull));
        } else if id == *i.push.id() {
            ipc::request_window(Some(Mode::Push));
        } else if id == *i.twoway.id() {
            ipc::request_window(Some(Mode::TwoWay));
        } else if id == *i.auto.id() {
            let mut cfg = Config::load();
            cfg.auto_sync = i.auto.is_checked();
            log::info(format!("Auto-Sync {}", if cfg.auto_sync { "on" } else { "off" }));
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
            UserEvent::Menu(m) => self.handle_menu(m, el),
            UserEvent::Tray(TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }) => {
                ipc::request_window(None)
            }
            UserEvent::Tray(TrayIconEvent::DoubleClick { .. }) => ipc::request_window(None),
            UserEvent::Tray(_) => {}
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

fn idle_status(cfg: &Config) -> (Status, String) {
    if cfg.pairs.is_empty() {
        (Status::Off, "no folder pairs set up".into())
    } else if cfg.auto_sync {
        (Status::Idle, format!("Auto-sync on (every {} min)", cfg.interval_minutes))
    } else {
        (Status::Off, "Auto-sync off".into())
    }
}

fn worker(rx: mpsc::Receiver<WorkerCmd>, shared: Arc<Mutex<Shared>>, proxy: EventLoopProxy<UserEvent>) {
    let mut cfg = Config::load();
    let mut mtime = Config::mtime();
    let mut last_run: Option<Instant> = None;
    let mut last_note = String::new();
    loop {
        let mut force = false;
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(WorkerCmd::RunNow) => force = true,
            Ok(WorkerCmd::Quit) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let m = Config::mtime();
        if m != mtime {
            mtime = m;
            let new = Config::load();
            let auto_changed = new.auto_sync != cfg.auto_sync;
            cfg = new;
            shared.lock().unwrap().auto = cfg.auto_sync;
            if auto_changed {
                last_run = None; // turning auto-sync on runs right away
            }
            let s = shared.lock().unwrap().status;
            if s != Status::Attention && s != Status::Error {
                let (st, t) = idle_status(&cfg);
                set(&shared, &proxy, st, t);
            } else {
                let _ = proxy.send_event(UserEvent::Refresh);
            }
        }
        let interval = Duration::from_secs(cfg.interval_minutes.max(1) as u64 * 60);
        let due = cfg.auto_sync && last_run.is_none_or(|t| t.elapsed() >= interval);
        if !(due || force) {
            continue;
        }
        last_run = Some(Instant::now());
        let pairs: Vec<_> = cfg.pairs.iter().filter(|p| p.enabled).cloned().collect();
        if pairs.is_empty() {
            continue;
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
                    errors.push(format!("{}: {e}", p.label()));
                }
            }
        }
        let now = chrono::Local::now().format("%H:%M");
        let (status, text, note) = if !errors.is_empty() {
            (Status::Error, format!("Auto-sync error ({now}), see log"), format!("Sync error:\n{}", errors[0]))
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

pub fn run() -> Result<()> {
    let Some(_tray_lock) = ipc::try_lock("tray.lock") else {
        // Already running: just open the window.
        ipc::request_window(None);
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
        let _ = proxy.send_event(UserEvent::Tray(e));
    }));
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(UserEvent::Menu(e));
    }));

    let cfg = Config::load();
    let (st, text) = idle_status(&cfg);
    let shared = Arc::new(Mutex::new(Shared { status: st, text, auto: cfg.auto_sync }));

    let items = Items {
        status: MenuItem::new("", false, None),
        open: MenuItem::new("Open Draisine", true, None),
        diff: MenuItem::new("Diff local ↔ remote …", true, None),
        pull: MenuItem::new("One-way sync remote ➡ local …", true, None),
        push: MenuItem::new("One-way sync local ➡ remote …", true, None),
        twoway: MenuItem::new("Two-way sync local ↔ remote …", true, None),
        auto: CheckMenuItem::new("Auto-Sync", true, cfg.auto_sync, None),
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
        &items.auto,
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
        ipc::request_window(None);
    }
    let mut app = App { tray: None, items, menu, shared, worker: tx };
    event_loop.run_app(&mut app)?;
    Ok(())
}
