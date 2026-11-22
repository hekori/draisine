//! Window process (started by the tray with `--window`).

mod browser;
mod preview;
mod theme;
mod wizard;

use crate::config::{APP_NAME, Config, Pair, dirs};
use crate::engine::{self, Action, Mode, Plan, Report};
use crate::rclone::{Cancel, Entry, Rclone};
use crate::{icon, ipc, log};
use browser::RemoteBrowser;
use eframe::egui::{self, Color32, CursorIcon, Layout, Margin, RichText, Ui, vec2};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use theme::{Pal, pal};
use wizard::{Outcome, Step, Wizard};

// ------------------------------------------------------------------ background jobs

/// Result of a closure running on a background thread; poll it every frame.
pub struct Bg<T> {
    slot: Arc<Mutex<Option<T>>>,
}

impl<T: Send + 'static> Bg<T> {
    pub fn spawn(ctx: &egui::Context, f: impl FnOnce() -> T + Send + 'static) -> Bg<T> {
        let slot = Arc::new(Mutex::new(None));
        let (s, ctx) = (slot.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = f();
            *s.lock().unwrap() = Some(r);
            ctx.request_repaint();
        });
        Bg { slot }
    }
    pub fn poll(&mut self) -> Option<T> {
        self.slot.lock().unwrap().take()
    }
}

// ------------------------------------------------------------------ state

#[derive(PartialEq, Clone, Copy)]
enum Page {
    Overview,
    Connections,
    Log,
    Settings,
    About,
}

type Plans = Vec<Result<Plan, String>>;
type Results = Vec<Result<(String, Report), String>>;
/// rclone version line + configured remotes (name, type).
type RcloneInfo = Result<(String, Vec<(String, String)>), String>;

enum Job {
    None,
    Planning(Mode),
    Review(Mode, Plans),
    Executing(Mode),
    Done(Mode, Results),
}

struct Edit {
    index: usize,
    draft: Pair,
    browse: bool,
    browser: RemoteBrowser,
}

struct App {
    cfg: Config,
    saved: Config,
    page: Page,
    job: Job,
    wizard: Option<Wizard>,
    plans_job: Option<Bg<(Mode, Plans)>>,
    /// Set by “Cancel” while planning: kills the running rclone and drops the result.
    plan_cancel: Option<Cancel>,
    exec_job: Option<Bg<Results>>,
    progress: Arc<Mutex<Vec<String>>>,
    confirm: bool,
    filter: String,
    view: preview::View,
    /// Summary chip clicked: show only this kind of change.
    only: Option<Action>,
    show_same: bool,
    diffs: preview::Diffs,
    log_text: String,
    log_loaded: Instant,
    last_poll: Instant,
    rinfo: Option<RcloneInfo>,
    rinfo_job: Option<Bg<RcloneInfo>>,
    tests: HashMap<String, Result<(), String>>,
    test_jobs: HashMap<String, Bg<Result<(), String>>>,
    edit: Option<Edit>,
    delete_pair: Option<usize>,
    logo: egui::TextureHandle,
    flash: Option<(String, Instant)>,
    /// Pairs whose two-way sync is being checked inline on the overview (no preview screen).
    syncing: Vec<String>,
    /// Pairs that turned out to be in sync, with the time (note fades out on the overview).
    in_sync: Option<(Vec<String>, Instant)>,
}

/// How long the “Everything in sync” note stays on a card (the last second fades out).
const IN_SYNC_NOTE: Duration = Duration::from_secs(4);

pub fn run(action: Option<Mode>) -> anyhow::Result<()> {
    let Some(_lock) = ipc::try_lock("window.lock") else {
        ipc::request_window(action);
        return Ok(());
    };
    let _ = ipc::take_request();
    let px = icon::rgba(64, icon::Status::Idle.color());
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_app_id("draisine")
            .with_inner_size([1120.0, 760.0])
            .with_min_inner_size([860.0, 560.0])
            .with_icon(egui::IconData { rgba: px, width: 64, height: 64 }),
        ..Default::default()
    };
    eframe::run_native(APP_NAME, opts, Box::new(move |cc| Ok(Box::new(App::new(cc, action)))))
        .map_err(|e| anyhow::anyhow!("Window: {e}"))
}

// ------------------------------------------------------------------ formatting helpers

fn human(n: i64) -> String {
    let n = n as f64;
    match n {
        n if n < 1024.0 => format!("{n} B"),
        n if n < 1024.0 * 1024.0 => format!("{:.1} KB", n / 1024.0),
        n if n < 1024.0 * 1024.0 * 1024.0 => format!("{:.1} MB", n / 1048576.0),
        n => format!("{:.2} GB", n / 1073741824.0),
    }
}

fn when(e: &Entry) -> String {
    chrono::DateTime::from_timestamp_millis(e.mtime_ms)
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn action_color(p: &Pal, a: Action) -> Color32 {
    match a {
        Action::DeleteLocal | Action::DeleteRemote => p.red,
        Action::OverwriteLocal | Action::OverwriteRemote | Action::Conflict | Action::Different => p.orange,
        Action::CopyToLocal | Action::CopyToRemote => p.green,
        Action::RenameLocal | Action::RenameRemote | Action::OnlyLocal | Action::OnlyRemote => p.blue,
    }
}

const GROUPS: [(Action, &str, &str); 12] = [
    (
        Action::Conflict,
        "Conflicts",
        "Changed on both sides. The cloud version keeps the name, yours is kept as a conflicted copy.",
    ),
    (Action::DeleteLocal, "Delete on this computer", "The files go to the trash."),
    (Action::DeleteRemote, "Delete in the cloud", "They go to the backup folder or the provider's trash."),
    (Action::OverwriteLocal, "Replace on this computer", "The old version goes to the trash."),
    (Action::OverwriteRemote, "Replace in the cloud", "Old version: backup folder or the provider's version history."),
    (Action::RenameLocal, "Rename on this computer", "Renamed in the cloud."),
    (Action::RenameRemote, "Rename in the cloud", "Renamed on this computer."),
    (Action::CopyToRemote, "Upload", "New in the cloud."),
    (Action::CopyToLocal, "Download", "New on this computer."),
    (Action::Different, "Different", "Same name, different content."),
    (Action::OnlyLocal, "Only on this computer", ""),
    (Action::OnlyRemote, "Only in the cloud", ""),
];

fn mode_title(m: Mode) -> &'static str {
    match m {
        Mode::Diff => "Differences",
        Mode::Pull => "Cloud ➡ this computer",
        Mode::Push => "This computer ➡ cloud",
        Mode::TwoWay => "Sync",
    }
}

// ------------------------------------------------------------------ app

impl App {
    fn new(cc: &eframe::CreationContext, action: Option<Mode>) -> App {
        theme::install(&cc.egui_ctx);
        let logo = cc.egui_ctx.load_texture(
            "logo",
            egui::ColorImage::from_rgba_unmultiplied([128, 128], &icon::rgba(128, [0x2e, 0x9e, 0x6b])),
            egui::TextureOptions::LINEAR,
        );
        let cfg = Config::load();
        let mut app = App {
            saved: cfg.clone(),
            wizard: cfg.pairs.is_empty().then(|| Wizard::new(Step::Welcome, false)),
            cfg,
            page: Page::Overview,
            job: Job::None,
            plans_job: None,
            plan_cancel: None,
            exec_job: None,
            progress: Arc::new(Mutex::new(Vec::new())),
            confirm: false,
            filter: String::new(),
            view: preview::View::Changes,
            only: None,
            show_same: false,
            diffs: HashMap::new(),
            log_text: String::new(),
            log_loaded: Instant::now() - Duration::from_secs(60),
            last_poll: Instant::now(),
            rinfo: None,
            rinfo_job: None,
            tests: HashMap::new(),
            test_jobs: HashMap::new(),
            edit: None,
            delete_pair: None,
            logo,
            flash: None,
            syncing: Vec::new(),
            in_sync: None,
        };
        app.load_rinfo(&cc.egui_ctx);
        #[cfg(debug_assertions)]
        if let Some(n) = std::env::var("DRAISINE_DEBUG_STEP").ok().and_then(|s| s.parse::<usize>().ok()) {
            app.wizard = Some(Wizard::debug_at(n));
        }
        #[cfg(debug_assertions)]
        if std::env::var("DRAISINE_DEBUG_THEME").as_deref() == Ok("light") {
            cc.egui_ctx.set_theme(egui::ThemePreference::Light);
        }
        #[cfg(debug_assertions)]
        if let Ok(pg) = std::env::var("DRAISINE_DEBUG_PAGE") {
            app.page = match pg.as_str() {
                "conn" => Page::Connections,
                "log" => Page::Log,
                "settings" => Page::Settings,
                "about" => Page::About,
                _ => Page::Overview,
            };
        }
        if let Some(m) = action {
            app.start_plan(&cc.egui_ctx, m, None);
        }
        app
    }

    fn busy(&self) -> bool {
        matches!(self.job, Job::Planning(_) | Job::Executing(_))
    }

    fn flash(&mut self, s: impl Into<String>) {
        self.flash = Some((s.into(), Instant::now()));
    }

    fn save(&mut self) {
        match self.cfg.save() {
            Ok(()) => self.saved = self.cfg.clone(),
            Err(e) => self.flash(format!("Saving failed: {e}")),
        }
    }

    fn load_rinfo(&mut self, ctx: &egui::Context) {
        let cfg = self.cfg.clone();
        self.rinfo_job = Some(Bg::spawn(ctx, move || {
            let rc = Rclone::new(&cfg).map_err(|e| e.to_string())?;
            Ok((format!("{}  ·  {}", rc.version(), rc.bin.display()), rc.remotes()))
        }));
    }

    /// Plans `mode` for the given pairs (None = all active pairs).
    fn start_plan(&mut self, ctx: &egui::Context, mode: Mode, ids: Option<Vec<String>>) {
        if self.busy() {
            return;
        }
        let pairs: Vec<Pair> = self
            .cfg
            .pairs
            .iter()
            .filter(|p| match &ids {
                Some(ids) => ids.contains(&p.id),
                None => p.enabled,
            })
            .cloned()
            .collect();
        self.page = Page::Overview;
        self.wizard = None;
        self.confirm = false;
        self.filter.clear();
        self.view = preview::View::Changes;
        self.only = None;
        self.show_same = false;
        self.diffs.clear();
        self.in_sync = None;
        self.syncing = if mode == Mode::TwoWay { pairs.iter().map(|p| p.id.clone()).collect() } else { Vec::new() };
        if pairs.is_empty() {
            self.job = Job::Review(mode, vec![Err("No active folder pair. Add one in the overview.".into())]);
            return;
        }
        self.job = Job::Planning(mode);
        log::info(format!("Preview: {}", mode.label()));
        let cfg = self.cfg.clone();
        let cancel: Cancel = Default::default();
        self.plan_cancel = Some(cancel.clone());
        self.plans_job = Some(Bg::spawn(ctx, move || {
            // Lets the tray show ↻ while we compare; released when this closure returns.
            let _sync_lock = ipc::hold_sync_lock();
            let plans: Vec<Result<Plan, String>> = pairs
                .iter()
                .map(|p| {
                    engine::plan_with(&cfg, p, mode, Some(cancel.clone())).map_err(|e| {
                        log::error(format!("[{}] Preview failed: {e:#}", p.label()));
                        format!("{e:#}")
                    })
                })
                .collect();
            if !cancel.load(std::sync::atomic::Ordering::Relaxed) {
                let note = if plans.iter().any(|p| p.is_err()) {
                    ipc::TrayNote::Changed // let the tray sort the error (offline, sign-in, …)
                } else if plans.iter().flatten().all(|p| p.count(|i| i.action.executable()) == 0) {
                    ipc::TrayNote::InSync
                } else {
                    ipc::TrayNote::Pending
                };
                ipc::notify_tray(note);
            }
            (mode, plans)
        }));
    }

    /// Stops a running comparison: kills rclone, discards the result, back to the overview.
    fn cancel_plan(&mut self) {
        if let Some(c) = self.plan_cancel.take() {
            c.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        // The worker thread finishes on its own within ~100 ms; its result is ignored.
        self.plans_job = None;
        self.job = Job::None;
        self.syncing.clear();
        log::info("Comparison cancelled");
        self.flash("Comparison cancelled");
    }

    fn start_execute(&mut self, ctx: &egui::Context) {
        let Job::Review(mode, plans) = &self.job else { return };
        let mode = *mode;
        let plans: Vec<Plan> = plans.iter().filter_map(|p| p.as_ref().ok().cloned()).collect();
        self.job = Job::Executing(mode);
        self.confirm = false;
        self.progress.lock().unwrap().clear();
        let (cfg, prog) = (self.cfg.clone(), self.progress.clone());
        self.exec_job = Some(Bg::spawn(ctx, move || {
            // Lets the tray show ↻ while we sync; released when this closure returns.
            let _sync_lock = ipc::hold_sync_lock();
            let mut results = Vec::new();
            for p in &plans {
                let label = p.pair.label();
                prog.lock().unwrap().push(format!("── {label}"));
                let r = engine::execute(&cfg, p, &mut |l| {
                    let mut g = prog.lock().unwrap();
                    g.push(l.to_string());
                    if g.len() > 500 {
                        g.drain(..100);
                    }
                });
                results.push(match r {
                    Ok(rep) => Ok((label, rep)),
                    Err(e) => {
                        log::error(format!("[{label}] {e:#}"));
                        Err(format!("{label}: {e:#}"))
                    }
                });
            }
            ipc::notify_tray(ipc::TrayNote::Changed);
            results
        }));
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(j) = &mut self.plans_job
            && let Some((m, plans)) = j.poll() {
                self.plans_job = None;
                self.plan_cancel = None;
                #[cfg(debug_assertions)]
                if std::env::var("DRAISINE_DEBUG_CONFIRM").is_ok() {
                    self.confirm = true;
                }
                let ids = std::mem::take(&mut self.syncing);
                let nothing_to_do = m == Mode::TwoWay
                    && plans.iter().all(|p| p.as_ref().is_ok_and(|p| p.count(|i| i.action.executable()) == 0));
                if nothing_to_do {
                    // No preview screen: just a short note on the overview cards.
                    self.job = Job::None;
                    self.in_sync = Some((ids, Instant::now()));
                } else {
                    self.job = Job::Review(m, plans);
                }
                #[cfg(debug_assertions)]
                if std::env::var("DRAISINE_DEBUG_VIEW").as_deref() == Ok("result") {
                    self.view = preview::View::Result;
                }
            }
        if let Some(j) = &mut self.exec_job
            && let Some(r) = j.poll() {
                self.exec_job = None;
                if let Job::Executing(m) = self.job {
                    self.job = Job::Done(m, r);
                }
            }
        if let Some(j) = &mut self.rinfo_job
            && let Some(r) = j.poll() {
                self.rinfo_job = None;
                self.rinfo = Some(r);
            }
        let done: Vec<String> = self
            .test_jobs
            .iter_mut()
            .filter_map(|(k, j)| j.poll().map(|r| (k.clone(), r)))
            .map(|(k, r)| {
                self.tests.insert(k.clone(), r);
                k
            })
            .collect();
        for k in done {
            self.test_jobs.remove(&k);
        }
        if self.last_poll.elapsed() > Duration::from_millis(500) {
            self.last_poll = Instant::now();
            if let Some(req) = ipc::take_request() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                if let Some(m) = req {
                    self.start_plan(ctx, m, None);
                }
            }
            // Auto-sync may be toggled from the tray.
            let disk = Config::load();
            if disk.auto_sync != self.saved.auto_sync {
                self.saved.auto_sync = disk.auto_sync;
                self.cfg.auto_sync = disk.auto_sync;
            }
        }
        ctx.request_repaint_after(Duration::from_millis(if self.busy() { 200 } else { 500 }));
    }

    // ------------------------------------------------------------------ sidebar

    fn sidebar(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.add(egui::Image::new((self.logo.id(), vec2(34.0, 34.0))));
            ui.label(theme::bold("Draisine", 20.0));
        });
        ui.add_space(18.0);
        let pending = match &self.job {
            Job::Review(m, _) if *m != Mode::Diff => 1,
            _ => 0,
        };
        for (pg, icon, label, badge) in [
            (Page::Overview, "🏠", "Overview", pending),
            (Page::Connections, "☁", "Connections", 0),
            (Page::Log, "📄", "Log", 0),
            (Page::Settings, "⚙", "Settings", 0),
            (Page::About, "ℹ", "About", 0),
        ] {
            if theme::nav_item(ui, icon, label, self.page == pg, badge).clicked() {
                self.page = pg;
                self.edit = None;
            }
        }
        // Auto-sync box + version, pinned to the bottom of the sidebar.
        ui.add_space((ui.available_height() - 118.0).max(12.0));
        egui::Frame::new().fill(p.surface).stroke(egui::Stroke::new(1.0, p.border)).corner_radius(10).inner_margin(12).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let mut on = self.cfg.auto_sync;
                if theme::toggle(ui, &mut on).changed() {
                    self.cfg.auto_sync = on;
                    log::info(format!("Auto-Sync {}", if on { "on" } else { "off" }));
                    self.save();
                }
                ui.label(theme::bold("Auto-Sync", 14.0));
            });
            let t = if self.cfg.auto_sync {
                format!("every {} min in the background", self.cfg.interval_minutes)
            } else {
                "off: manual only".into()
            };
            ui.label(theme::muted(ui, t).small());
        });
        ui.add_space(6.0);
        ui.label(theme::muted(ui, format!("Version {}", env!("CARGO_PKG_VERSION"))).small());
    }

    // ------------------------------------------------------------------ overview

    fn overview(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        match &self.job {
            // A two-way sync is checked inline: the overview stays, the cards show a spinner.
            Job::Planning(Mode::TwoWay) => {}
            Job::Planning(m) => {
                theme::page_header(ui, mode_title(*m), "");
                theme::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(theme::bold("Comparing this computer with the cloud …", 15.0));
                    });
                    ui.label(theme::muted(ui, "With many files this can take a while. Nothing is changed yet."));
                    ui.add_space(8.0);
                    if theme::secondary(ui, "Cancel").on_hover_text("Stop comparing. Nothing has been changed.").clicked() {
                        self.cancel_plan();
                    }
                });
                return;
            }
            Job::Review(..) => return self.review(ui),
            Job::Executing(m) => {
                let m = *m;
                theme::page_header(ui, mode_title(m), "");
                let lines = self.progress.lock().unwrap().clone();
                theme::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(theme::bold("Running …", 16.0));
                    });
                    let last = lines.iter().rev().find(|l| !l.starts_with("──")).cloned().unwrap_or_default();
                    ui.label(theme::muted(ui, last));
                    ui.add_space(6.0);
                    egui::CollapsingHeader::new("Details").default_open(false).show(ui, |ui| {
                        egui::ScrollArea::vertical().max_height(320.0).stick_to_bottom(true).show(ui, |ui| {
                            for l in &lines {
                                ui.label(RichText::new(l).monospace().size(12.0));
                            }
                        });
                    });
                });
                return;
            }
            Job::Done(..) => return self.done(ui),
            Job::None => {}
        }

        theme::page_header(ui, "Overview", "Your synced folders");
        if self.cfg.pairs.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.add(egui::Image::new((self.logo.id(), vec2(96.0, 96.0))));
                ui.add_space(8.0);
                ui.label(theme::bold("No folders connected yet", 22.0));
                ui.label(theme::muted(ui, "The setup assistant walks you through everything in a few steps."));
                ui.add_space(12.0);
                if theme::primary(ui, "✨  Start setup").clicked() {
                    self.wizard = Some(Wizard::new(Step::Welcome, false));
                }
            });
            return;
        }
        let ctx = ui.ctx().clone();
        let busy = self.busy();
        let any_enabled = self.cfg.pairs.iter().any(|p| p.enabled) && !busy;
        let syncing = self.syncing.clone();
        let in_sync_alpha = match &self.in_sync {
            Some((_, t)) if t.elapsed() < IN_SYNC_NOTE => {
                ctx.request_repaint();
                (IN_SYNC_NOTE - t.elapsed()).as_secs_f32().min(1.0)
            }
            Some(_) => {
                self.in_sync = None;
                0.0
            }
            None => 0.0,
        };
        let in_sync_ids = self.in_sync.as_ref().map(|x| x.0.clone()).unwrap_or_default();
        let mut cancel_sync = false;
        ui.horizontal(|ui| {
            if theme::primary_enabled(ui, any_enabled, "↔  Sync all").clicked() {
                self.start_plan(&ctx, Mode::TwoWay, None);
            }
            if theme::secondary_enabled(ui, any_enabled, "🔍  Compare all").clicked() {
                self.start_plan(&ctx, Mode::Diff, None);
            }
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::secondary(ui, "➕  Add folder pair").clicked() {
                    self.edit = None;
                    self.wizard = Some(Wizard::new(Step::Connection, false));
                }
            });
        });
        ui.add_space(6.0);
        let remotes: Vec<String> = match &self.rinfo {
            Some(Ok((_, r))) => r.iter().map(|x| x.0.clone()).collect(),
            _ => Vec::new(),
        };
        let cfg_snapshot = self.cfg.clone();
        let mut action: Option<(Mode, String)> = None;
        let (mut toggled, mut save_edit, mut cancel_edit) = (false, false, false);
        let (mut start_edit, mut reset) = (None, None);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (i, pair) in self.cfg.pairs.iter_mut().enumerate() {
                theme::card(ui, |ui| {
                    if let Some(ed) = self.edit.as_mut().filter(|e| e.index == i) {
                        (save_edit, cancel_edit) = edit_form(ui, &p, ed, pair, &remotes, &cfg_snapshot);
                        return;
                    }
                    ui.horizontal(|ui| {
                        ui.label(theme::bold(pair.label(), 17.0));
                        if !pair.enabled {
                            theme::badge(ui, "paused", p.muted);
                        }
                        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                            let id = pair.id.clone();
                            ui.menu_button(RichText::new("•••").size(13.0), |ui| {
                                for (m, label, hint) in [
                                    (Mode::Diff, "🔍  Compare", "Only show differences, change nothing"),
                                    (Mode::Pull, "⬇  Cloud ➡ this computer", "One-way: this computer becomes a mirror of the cloud"),
                                    (Mode::Push, "⬆  This computer ➡ cloud", "One-way: the cloud becomes a mirror of this computer"),
                                ] {
                                    if ui.button(label).on_hover_text(hint).clicked() {
                                        action = Some((m, id.clone()));
                                        ui.close();
                                    }
                                }
                                ui.separator();
                                if ui.button("✏  Edit").clicked() {
                                    start_edit = Some(i);
                                    ui.close();
                                }
                                if ui
                                    .button("↺  Reset sync state")
                                    .on_hover_text("Forgets the last sync. The next sync behaves like the first one: nothing is deleted.")
                                    .clicked()
                                {
                                    reset = Some(i);
                                    ui.close();
                                }
                                ui.separator();
                                if ui.button(RichText::new("🗑  Remove").color(p.red)).clicked() {
                                    self.delete_pair = Some(i);
                                    ui.close();
                                }
                            })
                            .response
                            .on_hover_text("Compare, one-way sync, edit, remove, …");
                            ui.add_space(4.0);
                            if theme::toggle(ui, &mut pair.enabled)
                                .on_hover_text("Active: included in “Sync all”, “Compare all” and auto-sync")
                                .changed()
                            {
                                toggled = true;
                            }
                        });
                    });
                    theme::path_line(ui, "💻", &pair.local);
                    theme::path_line(ui, "☁", &pair.remote);
                    if !pair.excludes.is_empty() {
                        ui.label(theme::muted(ui, format!("Excluded: {}", pair.excludes.join(", "))).small());
                    }
                    if !pair.remote_backup.is_empty() {
                        ui.label(theme::muted(ui, format!("Backup: {}", pair.remote_backup)).small());
                    }
                    let (t, r) = engine::last_status(pair);
                    match t {
                        Some(t) => ui.label(RichText::new(format!("🕓  Last synced {t}  ·  {r}")).size(13.0)),
                        None => ui.label(RichText::new("🕓  Never synced").size(13.0).color(p.orange)),
                    };
                    ui.add_space(4.0);
                    if syncing.contains(&pair.id) {
                        ui.horizontal(|ui| {
                            ui.set_min_height(34.0);
                            ui.spinner();
                            ui.label(theme::bold("Syncing …", 14.5))
                                .on_hover_text("Comparing this computer with the cloud. Nothing is changed yet.");
                            ui.add_space(6.0);
                            if theme::link(ui, "Cancel").clicked() {
                                cancel_sync = true;
                            }
                        });
                    } else {
                        ui.horizontal(|ui| {
                            if theme::primary_enabled(ui, !busy, "↔  Sync")
                                .on_hover_text("Two-way sync: take over changes from both sides")
                                .clicked()
                            {
                                action = Some((Mode::TwoWay, pair.id.clone()));
                            }
                            if in_sync_alpha > 0.0 && in_sync_ids.contains(&pair.id) {
                                ui.add_space(8.0);
                                ui.label(
                                    theme::bold("✔  Everything in sync, nothing to do.", 14.0)
                                        .color(p.green.gamma_multiply(in_sync_alpha)),
                                );
                            }
                        });
                    }
                });
                ui.add_space(4.0);
            }
        });
        if toggled {
            self.save();
        }
        if cancel_sync {
            self.cancel_plan();
        }
        if let Some(i) = start_edit {
            let d = self.cfg.pairs[i].clone();
            self.edit = Some(Edit { index: i, browser: RemoteBrowser::new("", ""), draft: d, browse: false });
        }
        if cancel_edit {
            self.edit = None;
        }
        if save_edit
            && let Some(mut ed) = self.edit.take() {
                ed.draft.excludes = ed.draft.excludes.iter().map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).collect();
                ed.draft.local = ed.draft.local.trim().into();
                ed.draft.remote = ed.draft.remote.trim().into();
                ed.draft.remote_backup = ed.draft.remote_backup.trim().into();
                log::info(format!("Folder pair changed: {}", ed.draft.label()));
                self.cfg.pairs[ed.index] = ed.draft;
                self.save();
                self.flash("Saved");
            }
        if let Some(i) = reset {
            engine::reset_state(&self.cfg.pairs[i]);
            self.flash("Sync state reset");
        }
        if let Some((m, id)) = action
            && !busy
        {
            self.start_plan(&ctx, m, Some(vec![id]));
        }
    }

    // ------------------------------------------------------------------ review

    fn action_bar(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let ctx = ui.ctx().clone();
        let Job::Review(mode, plans) = &self.job else { return };
        let mode = *mode;
        let ok: Vec<&Plan> = plans.iter().filter_map(|x| x.as_ref().ok()).collect();
        let total: usize = ok.iter().map(|p| p.items.len()).sum();
        let sel: usize = ok.iter().map(|p| p.count(|i| i.selected && i.action.executable())).sum();
        let destr: usize = ok.iter().map(|p| p.count(|i| i.selected && i.action.destructive())).sum();
        let ids: Vec<String> = ok.iter().map(|p| p.pair.id.clone()).collect();
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if mode == Mode::Diff {
                ui.label(theme::muted(ui, format!("{total} differences · nothing has been changed")));
            } else {
                ui.label(theme::bold(format!("{sel} of {total} changes selected"), 14.5));
                if destr > 0 {
                    theme::badge(ui, &format!("{destr} delete/replace"), p.red);
                }
            }
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                if mode == Mode::Diff {
                    if theme::primary_enabled(ui, total > 0, "↔  Sync now …").clicked() {
                        self.start_plan(&ctx, Mode::TwoWay, Some(ids.clone()));
                    }
                    if theme::secondary(ui, "Close").clicked() {
                        self.job = Job::None;
                    }
                } else {
                    let label = if sel == 0 { "Apply".to_string() } else { format!("▶  Apply {sel} changes") };
                    if theme::primary_enabled(ui, sel > 0, &label).clicked() {
                        if destr > 0 {
                            self.confirm = true;
                        } else {
                            self.start_execute(&ctx);
                        }
                    }
                    if theme::secondary(ui, "⟳  Check again").clicked() {
                        self.start_plan(&ctx, mode, Some(ids.clone()));
                    }
                    if theme::link(ui, "Cancel").clicked() {
                        self.job = Job::None;
                    }
                }
            });
        });
        ui.add_space(10.0);
    }

    fn review(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let App { job, cfg, filter, view, only, show_same, diffs, .. } = self;
        let Job::Review(mode, plans) = job else { return };
        let mode = *mode;
        let sub = if mode == Mode::Diff {
            "View only: nothing will be changed."
        } else {
            "Nothing has been changed yet. Check the list, then click “Apply” below."
        };
        theme::page_header(ui, &format!("Preview: {}", mode_title(mode)), sub);

        // Summary chips (click: show only that kind of change)
        ui.horizontal_wrapped(|ui| {
            for (a, title, desc) in GROUPS {
                let n: usize = plans.iter().filter_map(|x| x.as_ref().ok()).map(|pl| pl.count(|i| i.action == a)).sum();
                if n > 0 {
                    let active = only.map(|o| o == a);
                    let r = preview::chip(ui, &format!("{n}  {title}"), action_color(&p, a), active);
                    let r = if desc.is_empty() { r } else { r.on_hover_text(desc) };
                    if r.clicked() {
                        *only = if *only == Some(a) { None } else { Some(a) };
                    }
                }
            }
            let same: usize = plans.iter().filter_map(|x| x.as_ref().ok()).map(|pl| pl.in_sync.len()).sum();
            theme::badge(ui, &format!("{same}  already identical"), p.muted);
            if only.is_some() && theme::link(ui, "show all").clicked() {
                *only = None;
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(filter)
                    .hint_text("🔍  Filter files …")
                    .desired_width(320.0)
                    .margin(Margin::symmetric(10, 6)),
            );
            if mode != Mode::Diff {
                ui.add_space(12.0);
                ui.selectable_value(view, preview::View::Changes, "Changes")
                    .on_hover_text("What happens on each side, and why");
                ui.selectable_value(view, preview::View::Result, "Result")
                    .on_hover_text("Which files you will have after applying, and what they contain");
            }
        });
        let text = filter.to_lowercase();
        let f = preview::Filter { text: &text, only: *only };
        let view = if mode == Mode::Diff { preview::View::Changes } else { *view };
        ui.add_space(4.0);

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for plan in plans.iter_mut() {
                let plan = match plan {
                    Ok(pl) => pl,
                    Err(e) => {
                        theme::callout(ui, p.red, |ui| {
                            ui.label(theme::bold("Comparison failed", 15.0).color(p.red));
                            ui.label(e.as_str());
                        });
                        ui.add_space(6.0);
                        continue;
                    }
                };
                theme::card(ui, |ui| {
                    ui.label(theme::bold(plan.pair.label(), 17.0));
                    theme::path_line(ui, "💻", &format!("{}   ·  {} files", plan.pair.local, plan.local_files));
                    theme::path_line(ui, "☁", &format!("{}   ·  {} files", plan.pair.remote, plan.remote_files));
                    for w in &plan.warnings {
                        theme::callout(ui, p.orange, |ui| {
                            ui.label(RichText::new(format!("⚠  {w}")).color(p.orange));
                        });
                    }
                    if plan.items.is_empty() {
                        ui.add_space(4.0);
                        ui.label(theme::bold("✔  Everything in sync, nothing to do.", 15.0).color(p.green));
                        return;
                    }
                    ui.add_space(8.0);
                    match view {
                        preview::View::Changes => preview::changes(ui, &p, cfg, plan, mode, &f, diffs),
                        preview::View::Result => preview::result(ui, &p, plan, &f, show_same),
                    }
                });
                ui.add_space(8.0);
            }
        });
    }

    fn confirm_modal(&mut self, ctx: &egui::Context) {
        if !self.confirm {
            return;
        }
        let Job::Review(_, plans) = &self.job else {
            self.confirm = false;
            return;
        };
        let items: Vec<(String, Action, String)> = plans
            .iter()
            .filter_map(|p| p.as_ref().ok())
            .flat_map(|p| p.items.iter().filter(|i| i.selected && i.action.destructive()).map(|i| (p.pair.label(), i.action, i.path.clone())))
            .collect();
        let mut go = None;
        egui::Modal::new(egui::Id::unique("draisine-confirm")).show(ctx, |ui| {
            let p = pal(ui);
            ui.set_width(560.0);
            ui.label(theme::bold("Really delete or replace?", 21.0));
            ui.add_space(6.0);
            for (a, what) in [
                (Action::DeleteLocal, "delete on this computer (➡ trash)"),
                (Action::OverwriteLocal, "replace on this computer (old version ➡ trash)"),
                (Action::DeleteRemote, "delete in the cloud"),
                (Action::OverwriteRemote, "replace in the cloud"),
            ] {
                let n = items.iter().filter(|i| i.1 == a).count();
                if n > 0 {
                    ui.horizontal(|ui| {
                        theme::badge(ui, &n.to_string(), action_color(&p, a));
                        ui.label(format!("files to {what}"));
                    });
                }
            }
            ui.add_space(6.0);
            egui::Frame::new().fill(p.surface2).corner_radius(8).inner_margin(10).show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    for (pair, a, path) in &items {
                        ui.label(RichText::new(format!("{}   {path}   ({pair})", a.label())).color(action_color(&p, *a)).size(12.5));
                    }
                });
            });
            ui.add_space(6.0);
            ui.label(theme::muted(
                ui,
                "Files deleted or replaced in the cloud go to the folder pair's backup folder (if set), \
                 otherwise to the provider's trash or version history.",
            ).small());
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::danger(ui, "Yes, apply").clicked() {
                    go = Some(true);
                }
                if theme::secondary(ui, "Cancel").clicked() {
                    go = Some(false);
                }
            });
        });
        match go {
            Some(true) => self.start_execute(ctx),
            Some(false) => self.confirm = false,
            None => {}
        }
    }

    fn done(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let Job::Done(mode, results) = &self.job else { return };
        let failed = results.iter().any(|r| r.as_ref().map(|x| !x.1.failed.is_empty()).unwrap_or(true));
        let conflicts: usize = results.iter().filter_map(|r| r.as_ref().ok()).map(|r| r.1.conflicts.len()).sum();
        theme::page_header(ui, mode_title(*mode), "");
        let mut close = false;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    let (icon, c, t) = if failed {
                        ("⚠", p.orange, "Done, but not everything worked")
                    } else {
                        ("✔", p.green, "Done")
                    };
                    ui.label(RichText::new(icon).size(30.0).color(c));
                    ui.label(theme::bold(t, 21.0));
                });
                ui.add_space(6.0);
                for r in results {
                    match r {
                        Ok((label, rep)) => {
                            ui.label(theme::bold(label, 15.0));
                            ui.label(theme::muted(ui, rep.summary()));
                            for f in &rep.failed {
                                ui.label(RichText::new(format!("✖  {f}")).color(p.red).size(13.0));
                            }
                        }
                        Err(e) => {
                            ui.label(RichText::new(format!("✖  {e}")).color(p.red));
                        }
                    }
                    ui.add_space(4.0);
                }
            });
            if conflicts > 0 {
                ui.add_space(8.0);
                theme::callout(ui, p.orange, |ui| {
                    ui.label(theme::bold(format!("⚠  {conflicts} conflict(s): both versions were kept"), 15.0).color(p.orange));
                    for r in results.iter().filter_map(|r| r.as_ref().ok()) {
                        for (path, copy) in &r.1.conflicts {
                            ui.label(RichText::new(format!("{path}   ➡ cloud version\n{copy}   ➡ your version")).size(13.0));
                        }
                    }
                    ui.add_space(4.0);
                    ui.label(
                        "To resolve a conflict: compare both files, copy what you need from the \
                         conflicted copy into the original and delete the copy. The next sync picks \
                         that up on both sides.",
                    );
                });
            }
            ui.add_space(12.0);
            if theme::primary(ui, "Done").clicked() {
                close = true;
            }
        });
        if close {
            self.job = Job::None;
        }
    }

    // ------------------------------------------------------------------ pairs

    fn delete_modal(&mut self, ctx: &egui::Context) {
        let Some(i) = self.delete_pair else { return };
        let Some(pair) = self.cfg.pairs.get(i).cloned() else {
            self.delete_pair = None;
            return;
        };
        let mut go = None;
        egui::Modal::new(egui::Id::unique("draisine-delete-pair")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(theme::bold(format!("Remove “{}”?", pair.label()), 20.0));
            ui.add_space(4.0);
            ui.label("The folder pair is only removed from Draisine. No files are deleted, neither on this computer nor in the cloud.");
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::danger(ui, "Remove").clicked() {
                    go = Some(true);
                }
                if theme::secondary(ui, "Cancel").clicked() {
                    go = Some(false);
                }
            });
        });
        match go {
            Some(true) => {
                log::info(format!("Folder pair removed: {}", pair.label()));
                engine::reset_state(&pair);
                self.cfg.pairs.remove(i);
                self.save();
                self.delete_pair = None;
            }
            Some(false) => self.delete_pair = None,
            None => {}
        }
    }

    // ------------------------------------------------------------------ connections

    fn connections_page(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        theme::page_header(ui, "Connections", "Cloud storage Draisine can exchange files with");
        ui.horizontal(|ui| {
            if theme::primary(ui, "➕  New connection").clicked() {
                self.wizard = Some(Wizard::new(Step::Connection, true));
            }
            if theme::secondary(ui, "⟳  Refresh").clicked() {
                self.tests.clear();
                self.load_rinfo(ui.ctx());
            }
        });
        ui.add_space(6.0);
        match &self.rinfo {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading …");
                });
            }
            Some(Err(e)) => {
                theme::callout(ui, p.red, |ui| {
                    ui.label(theme::bold("rclone not found", 15.0).color(p.red));
                    ui.label(e.as_str());
                });
            }
            Some(Ok((_, remotes))) => {
                if remotes.is_empty() {
                    theme::card(ui, |ui| {
                        ui.label(theme::bold("No connection yet", 16.0));
                        ui.label(theme::muted(ui, "Use “New connection” to add one, e.g. to Google Drive."));
                    });
                }
                let remotes = remotes.clone();
                for (name, typ) in remotes {
                    let used = self.cfg.pairs.iter().filter(|pp| pp.remote.split(':').next() == Some(name.as_str())).count();
                    theme::card(ui, |ui| {
                        ui.horizontal(|ui| {
                            theme::provider_icon(ui, &typ, 26.0);
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(theme::bold(&name, 16.0));
                                    // The logo already shows the provider; label only the others.
                                    if !theme::has_logo(&typ) {
                                        theme::badge(ui, &typ, p.blue);
                                    }
                                });
                                ui.label(theme::muted(ui, if used == 0 { "not used by any folder pair".to_string() } else { format!("used by {used} folder pair{}", if used == 1 { "" } else { "s" }) }));
                            });
                            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                                // Test automatically once; only offer a manual retry after a failure.
                                let start = !self.test_jobs.contains_key(&name) && !self.tests.contains_key(&name);
                                let mut retry = false;
                                if self.test_jobs.contains_key(&name) || start {
                                    ui.spinner();
                                    ui.label(theme::muted(ui, "checking …"));
                                } else {
                                    match self.tests.get(&name) {
                                        Some(Ok(())) => {
                                            theme::badge(ui, "✔ reachable", p.green);
                                        }
                                        Some(Err(e)) => {
                                            retry = theme::secondary(ui, "⟳  Try again").clicked();
                                            theme::badge(ui, "✖ Error", p.red).on_hover_text(e);
                                        }
                                        None => {}
                                    }
                                }
                                if start || retry {
                                    let (cfg, n) = (self.cfg.clone(), name.clone());
                                    self.tests.remove(&name);
                                    self.test_jobs.insert(
                                        name.clone(),
                                        Bg::spawn(ui.ctx(), move || Rclone::new(&cfg).map_err(|e| e.to_string())?.test_remote(&n).map_err(|e| e.to_string())),
                                    );
                                }
                            });
                        });
                    });
                    ui.add_space(4.0);
                }
                ui.add_space(6.0);
                ui.label(theme::muted(ui, "To change or delete connections, run “rclone config” in a terminal.").small());
            }
        }
    }

    // ------------------------------------------------------------------ log

    fn log_page(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        if self.log_loaded.elapsed() > Duration::from_secs(2) {
            self.log_text = log::tail(800);
            self.log_loaded = Instant::now();
        }
        theme::page_header(ui, "Log", "Everything Draisine does. One text file per day, nothing is deleted.");
        ui.horizontal(|ui| {
            if theme::secondary(ui, "📄  Open as TXT").clicked() {
                log::open_today();
            }
            if theme::secondary(ui, "📁  Open folder").clicked() {
                log::open_dir();
            }
        });
        ui.add_space(6.0);
        egui::Frame::new().fill(p.surface).stroke(egui::Stroke::new(1.0, p.border)).corner_radius(12).inner_margin(12).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_height(ui.available_height());
            egui::ScrollArea::both().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                for line in self.log_text.lines() {
                    let c = if line.contains(" ERROR ") {
                        p.red
                    } else if line.contains(" WARN ") {
                        p.orange
                    } else if line.contains(" rclone: ") {
                        p.muted
                    } else {
                        p.text
                    };
                    ui.add(egui::Label::new(RichText::new(line).monospace().size(12.0).color(c)).extend());
                }
            });
        });
    }

    // ------------------------------------------------------------------ settings

    fn settings_page(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        theme::page_header(ui, "Settings", "Changes are saved immediately");
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    theme::toggle(ui, &mut self.cfg.auto_sync);
                    ui.label(theme::bold("Sync automatically", 16.0));
                });
                ui.horizontal(|ui| {
                    ui.label("Every");
                    ui.add(egui::DragValue::new(&mut self.cfg.interval_minutes).range(1..=1440));
                    ui.label("minutes while Draisine is running in the tray.");
                });
                ui.label(theme::muted(ui, "New files and conflicts are applied automatically. Deletes and replacements always wait for your confirmation: the tray icon then turns orange."));
            });
            ui.add_space(6.0);
            theme::card(ui, |ui| {
                ui.label(theme::bold("Safety", 16.0));
                ui.horizontal(|ui| {
                    ui.label("Warn if more than");
                    ui.add(egui::DragValue::new(&mut self.cfg.max_delete_percent).range(1..=100).suffix(" %"));
                    ui.label("of the files would be deleted");
                });
                ui.horizontal(|ui| {
                    theme::toggle(ui, &mut self.cfg.check_download);
                    ui.label("Compare contents by download if needed (more accurate, but slow)");
                });
            });
            ui.add_space(6.0);
            theme::card(ui, |ui| {
                ui.label(theme::bold("rclone", 16.0));
                match &self.rinfo {
                    Some(Ok((v, _))) => ui.label(RichText::new(format!("✔  {v}")).color(p.green)),
                    Some(Err(e)) => ui.label(RichText::new(format!("✖  {e}")).color(p.red)),
                    None => ui.label(theme::muted(ui, "checking …")),
                };
                egui::CollapsingHeader::new("Advanced").default_open(false).show(ui, |ui| {
                    field(ui, "Program (empty = next to Draisine, otherwise PATH)");
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.rclone_path).desired_width(f32::INFINITY));
                    field(ui, "rclone.conf (empty = next to Draisine, otherwise rclone default)");
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.rclone_config).desired_width(f32::INFINITY));
                    field(ui, "Extra flags (one per line)");
                    let mut s = self.cfg.extra_flags.join("\n");
                    if ui.add(egui::TextEdit::multiline(&mut s).desired_rows(2).desired_width(f32::INFINITY)).changed() {
                        self.cfg.extra_flags = s.split('\n').map(str::to_string).collect();
                    }
                    if theme::secondary(ui, "⟳  Check again").clicked() {
                        self.load_rinfo(ui.ctx());
                    }
                });
            });
            ui.add_space(6.0);
            theme::card(ui, |ui| {
                ui.label(theme::bold("Storage locations", 16.0));
                let d = dirs();
                for (label, path) in [
                    ("Settings", d.config_file.parent().unwrap().to_path_buf()),
                    ("Sync state", d.state_dir.clone()),
                    ("Logs", d.log_dir.clone()),
                ] {
                    ui.horizontal(|ui| {
                        ui.label(format!("{label}:"));
                        if theme::link(ui, &path.display().to_string()).clicked() {
                            let _ = open::that_detached(&path);
                        }
                    });
                }
                ui.label(theme::muted(
                    ui,
                    if d.portable {
                        "Portable mode: everything is stored in the “draisine-data” folder next to the program.".to_string()
                    } else {
                        format!("Tip: a folder named “{}” next to the program turns on portable mode.", crate::config::PORTABLE_DIR)
                    },
                ).small());
            });
            ui.add_space(6.0);
            theme::card(ui, |ui| {
                ui.label(theme::bold("Setup", 16.0));
                if theme::secondary(ui, "✨  Run the setup assistant again").clicked() {
                    self.wizard = Some(Wizard::new(Step::Welcome, false));
                }
            });
        });
        // Settings are saved immediately (rclone-relevant changes re-check rclone).
        if self.cfg != self.saved {
            let rclone_changed = self.cfg.rclone_path != self.saved.rclone_path || self.cfg.rclone_config != self.saved.rclone_config;
            let mut c = self.cfg.clone();
            c.extra_flags.retain(|f| !f.trim().is_empty());
            if c.save().is_ok() {
                self.saved = self.cfg.clone();
            }
            if rclone_changed {
                self.load_rinfo(ui.ctx());
            }
        }
    }
}

const REPO_URL: &str = "https://github.com/hekori/draisine";
const LICENSE: &str = include_str!("../../LICENSE");
const THIRD_PARTY: &str = include_str!("../../assets/THIRD_PARTY.txt");

fn about_page(ui: &mut Ui) {
    theme::page_header(ui, "About", &format!("Draisine {}", env!("CARGO_PKG_VERSION")));
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        theme::card(ui, |ui| {
            ui.label(theme::bold("Draisine", 16.0));
            ui.label("Safe sync of local folders with Google Drive & co. Every delete and overwrite is previewed and needs your confirmation. rclone does the transfer.");
            ui.add_space(4.0);
            ui.label("Draisine is open source. Source code, issues and releases are on GitHub:");
            if theme::link(ui, REPO_URL).clicked() {
                let _ = open::that_detached(REPO_URL);
            }
            ui.add_space(4.0);
            ui.label(theme::muted(ui, "Copyright © 2026 Sebastian Walter. Released under the MIT License."));
            egui::CollapsingHeader::new("License text").default_open(false).show(ui, |ui| {
                ui.label(RichText::new(LICENSE).monospace().small());
            });
        });
        ui.add_space(6.0);
        theme::card(ui, |ui| {
            ui.label(theme::bold("Open-source attribution", 16.0));
            ui.label("Draisine is built on open-source software:");
            for (name, license, url) in [
                ("rclone (bundled)", "MIT", "https://rclone.org"),
                ("egui / eframe", "MIT or Apache-2.0", "https://github.com/emilk/egui"),
                ("Noto Sans font", "SIL Open Font License 1.1", "https://github.com/notofonts/latin-greek-cyrillic"),
                ("Material Design Icons", "Apache-2.0", "https://pictogrammers.com/library/mdi/"),
            ] {
                ui.horizontal(|ui| {
                    if theme::link(ui, name).clicked() {
                        let _ = open::that_detached(url);
                    }
                    ui.label(theme::muted(ui, license));
                });
            }
            let n = THIRD_PARTY.matches("\n  License:").count();
            ui.label(theme::muted(ui, format!("…and {n} components in total, including all Rust crates compiled into Draisine.")).small());
            egui::CollapsingHeader::new("All components and licenses").default_open(false).show(ui, |ui| {
                if theme::secondary(ui, "📋  Copy to clipboard").clicked() {
                    ui.ctx().copy_text(THIRD_PARTY.to_string());
                }
                ui.add_space(4.0);
                ui.label(RichText::new(THIRD_PARTY).monospace().small());
            });
        });
    });
}

/// Inline editor for a folder pair (shown inside its card on the overview).
/// Returns (save clicked, cancel clicked).
fn edit_form(ui: &mut Ui, p: &Pal, ed: &mut Edit, pair: &Pair, remotes: &[String], cfg: &Config) -> (bool, bool) {
    let (mut save, mut cancel) = (false, false);
    let d = &mut ed.draft;
        ui.label(theme::bold("Edit folder pair", 17.0));
        ui.add_space(4.0);
        field(ui, "Name");
        ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(f32::INFINITY));
        field(ui, "💻  Folder on this computer");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut d.local).desired_width(ui.available_width() - 130.0));
            if theme::secondary(ui, "📂  Choose …").clicked()
                && let Some(f) = rfd::FileDialog::new().pick_folder() {
                    d.local = f.display().to_string();
                }
        });
        field(ui, "☁  Folder in the cloud");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut d.remote).desired_width(ui.available_width() - 150.0));
            if theme::secondary(ui, if ed.browse { "Done" } else { "☁  Browse …" }).clicked() {
                ed.browse = !ed.browse;
                if ed.browse {
                    let (r, path) = d.remote.split_once(':').unwrap_or((remotes.first().map(String::as_str).unwrap_or(""), ""));
                    ed.browser = RemoteBrowser::new(r, path);
                }
            }
        });
        if ed.browse {
            ed.browser.ui(ui, cfg, 180.0);
            d.remote = ed.browser.full();
        }
        egui::CollapsingHeader::new("Advanced").default_open(false).show(ui, |ui| {
            field(ui, "Backup folder in the cloud (optional)");
            ui.add(
                egui::TextEdit::singleline(&mut d.remote_backup)
                    .desired_width(f32::INFINITY)
                    .hint_text("e.g. gdrive:_draisine-backup  (empty = provider's trash/versions)"),
            );
            ui.label(theme::muted(ui, "Must be on the same connection, but outside the synced folder.").small());
            field(ui, "Exclude (one pattern per line)");
            let mut s = d.excludes.join("\n");
            if ui.add(egui::TextEdit::multiline(&mut s).desired_rows(3).desired_width(f32::INFINITY).hint_text("e.g. 2020/SALARY/**\n*.tmp")).changed() {
                d.excludes = s.split('\n').map(str::to_string).collect();
            }
        });
        if d.signature() != pair.signature() {
            theme::callout(ui, p.blue, |ui| {
                ui.label(RichText::new("ℹ  Folders or excludes changed: the next sync behaves like a first sync (nothing is deleted).").size(13.0));
            });
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let ok = !d.local.trim().is_empty() && d.remote.contains(':');
            if theme::primary_enabled(ui, ok, "Save").clicked() {
                save = true;
            }
            if theme::secondary(ui, "Cancel").clicked() {
                cancel = true;
            }
        });
    (save, cancel)
}

fn field(ui: &mut Ui, label: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(label).size(13.0).color(pal(ui).muted));
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        let p = pal(ui);

        if let Some(w) = &mut self.wizard {
            let mut cfg = self.cfg.clone();
            w.poll(&ctx, &cfg);
            let out = egui::Panel::bottom("wizard-nav")
                .frame(egui::Frame::new().fill(p.bg).stroke(egui::Stroke::new(1.0, p.border)))
                .show(ui, |ui| w.footer(ui, &mut cfg))
                .inner;
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(p.bg).inner_margin(Margin::same(24)))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| w.body(ui, &mut cfg));
                });
            if cfg != self.cfg {
                self.cfg = cfg;
                self.saved = self.cfg.clone();
            }
            match out {
                Outcome::Continue => {}
                Outcome::Cancel => {
                    self.wizard = None;
                    self.load_rinfo(&ctx);
                }
                Outcome::Finished { diff_pair } => {
                    self.wizard = None;
                    self.load_rinfo(&ctx);
                    self.page = Page::Overview;
                    if let Some(id) = diff_pair {
                        self.start_plan(&ctx, Mode::Diff, Some(vec![id]));
                    }
                }
            }
            return;
        }

        egui::Panel::left("nav")
            .exact_size(220.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(p.sidebar).inner_margin(Margin::symmetric(12, 0)).stroke(egui::Stroke::new(1.0, p.border)))
            .show(ui, |ui| self.sidebar(ui));
        if self.page == Page::Overview && matches!(self.job, Job::Review(..)) {
            egui::Panel::bottom("actionbar")
                .frame(egui::Frame::new().fill(p.surface).inner_margin(Margin::symmetric(28, 0)).stroke(egui::Stroke::new(1.0, p.border)))
                .show(ui, |ui| self.action_bar(ui));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.bg).inner_margin(Margin { left: 28, right: 28, top: 22, bottom: 12 }))
            .show(ui, |ui| match self.page {
                Page::Overview => self.overview(ui),
                Page::Connections => self.connections_page(ui),
                Page::Log => self.log_page(ui),
                Page::Settings => self.settings_page(ui),
                Page::About => about_page(ui),
            });
        self.confirm_modal(&ctx);
        self.delete_modal(&ctx);
        if let Some((msg, t)) = &self.flash {
            if t.elapsed() > Duration::from_secs(3) {
                self.flash = None;
            } else {
                let msg = msg.clone();
                egui::Area::new(egui::Id::unique("flash")).anchor(egui::Align2::CENTER_BOTTOM, vec2(0.0, -24.0)).show(&ctx, |ui| {
                    egui::Frame::new().fill(p.text).corner_radius(20).inner_margin(Margin::symmetric(16, 8)).show(ui, |ui| {
                        ui.label(RichText::new(msg).color(p.bg));
                    });
                });
            }
        }
        let _ = CursorIcon::Default;
    }
}
