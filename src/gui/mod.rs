//! Window process (started by the tray with `--window`).

mod browser;
mod theme;
mod wizard;

use crate::config::{APP_NAME, Config, Pair, dirs};
use crate::engine::{self, Action, Mode, Plan, Report};
use crate::rclone::{Entry, Rclone};
use crate::{icon, ipc, log};
use browser::RemoteBrowser;
use eframe::egui::{self, Color32, CursorIcon, Layout, Margin, RichText, Sense, Ui, vec2};
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
    Pairs,
    Connections,
    Log,
    Settings,
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
    exec_job: Option<Bg<Results>>,
    progress: Arc<Mutex<Vec<String>>>,
    confirm: bool,
    filter: String,
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
}

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
        .map_err(|e| anyhow::anyhow!("Fenster: {e}"))
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
        .map(|t| t.with_timezone(&chrono::Local).format("%d.%m.%Y %H:%M").to_string())
        .unwrap_or_default()
}

fn action_color(p: &Pal, a: Action) -> Color32 {
    match a {
        Action::DeleteLocal | Action::DeleteRemote => p.red,
        Action::OverwriteLocal | Action::OverwriteRemote | Action::Conflict | Action::Different => p.orange,
        Action::CopyToLocal | Action::CopyToRemote => p.green,
        Action::OnlyLocal | Action::OnlyRemote => p.blue,
    }
}

const GROUPS: [(Action, &str, &str); 10] = [
    (Action::Conflict, "Konflikte", "Auf beiden Seiten geändert. Beide Versionen werden als Konfliktdateien behalten."),
    (Action::DeleteLocal, "Auf diesem Rechner löschen", "Die Dateien kommen in den Papierkorb."),
    (Action::DeleteRemote, "In der Cloud löschen", "Landen im Backup-Ordner bzw. Papierkorb des Anbieters."),
    (Action::OverwriteLocal, "Auf diesem Rechner ersetzen", "Die alte Version kommt in den Papierkorb."),
    (Action::OverwriteRemote, "In der Cloud ersetzen", "Alte Version: Backup-Ordner bzw. Versionen des Anbieters."),
    (Action::CopyToRemote, "Hochladen", "Neu in der Cloud."),
    (Action::CopyToLocal, "Herunterladen", "Neu auf diesem Rechner."),
    (Action::Different, "Unterschiedlich", "Gleicher Name, anderer Inhalt."),
    (Action::OnlyLocal, "Nur auf diesem Rechner", ""),
    (Action::OnlyRemote, "Nur in der Cloud", ""),
];

fn mode_title(m: Mode) -> &'static str {
    match m {
        Mode::Diff => "Unterschiede",
        Mode::Pull => "Cloud ➡ dieser Rechner",
        Mode::Push => "Dieser Rechner ➡ Cloud",
        Mode::TwoWay => "Synchronisieren",
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
            exec_job: None,
            progress: Arc::new(Mutex::new(Vec::new())),
            confirm: false,
            filter: String::new(),
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
                "pairs" => Page::Pairs,
                "conn" => Page::Connections,
                "log" => Page::Log,
                "settings" => Page::Settings,
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
            Err(e) => self.flash(format!("Speichern fehlgeschlagen: {e}")),
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
        if pairs.is_empty() {
            self.job = Job::Review(mode, vec![Err("Kein aktives Ordnerpaar. Lege unter „Ordnerpaare“ eines an.".into())]);
            return;
        }
        self.job = Job::Planning(mode);
        log::info(format!("Vorschau: {}", mode.label()));
        let cfg = self.cfg.clone();
        self.plans_job = Some(Bg::spawn(ctx, move || {
            let plans = pairs
                .iter()
                .map(|p| {
                    engine::plan(&cfg, p, mode).map_err(|e| {
                        log::error(format!("[{}] Vorschau fehlgeschlagen: {e:#}", p.label()));
                        format!("{e:#}")
                    })
                })
                .collect();
            (mode, plans)
        }));
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
            results
        }));
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(j) = &mut self.plans_job
            && let Some((m, plans)) = j.poll() {
                self.plans_job = None;
                #[cfg(debug_assertions)]
                if std::env::var("DRAISINE_DEBUG_CONFIRM").is_ok() {
                    self.confirm = true;
                }
                self.job = Job::Review(m, plans);
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
            (Page::Overview, "🏠", "Übersicht", pending),
            (Page::Pairs, "↔", "Ordnerpaare", 0),
            (Page::Connections, "☁", "Verbindungen", 0),
            (Page::Log, "📄", "Protokoll", 0),
            (Page::Settings, "⚙", "Einstellungen", 0),
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
                    log::info(format!("Auto-Sync {}", if on { "an" } else { "aus" }));
                    self.save();
                }
                ui.label(theme::bold("Auto-Sync", 14.0));
            });
            let t = if self.cfg.auto_sync {
                format!("alle {} min im Hintergrund", self.cfg.interval_minutes)
            } else {
                "aus: nur manuell".into()
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
            Job::Planning(m) => {
                theme::page_header(ui, mode_title(*m), "");
                theme::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(theme::bold("Vergleiche diesen Rechner mit der Cloud …", 15.0));
                    });
                    ui.label(theme::muted(ui, "Bei vielen Dateien kann das einen Moment dauern. Es wird noch nichts verändert."));
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
                        ui.label(theme::bold("Wird ausgeführt …", 16.0));
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

        theme::page_header(ui, "Übersicht", "Deine synchronisierten Ordner");
        if self.cfg.pairs.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.add(egui::Image::new((self.logo.id(), vec2(96.0, 96.0))));
                ui.add_space(8.0);
                ui.label(theme::bold("Noch keine Ordner verbunden", 22.0));
                ui.label(theme::muted(ui, "Der Einrichtungsassistent führt dich in wenigen Schritten durch alles."));
                ui.add_space(12.0);
                if theme::primary(ui, "✨  Einrichtung starten").clicked() {
                    self.wizard = Some(Wizard::new(Step::Welcome, false));
                }
            });
            return;
        }
        let ctx = ui.ctx().clone();
        let any_enabled = self.cfg.pairs.iter().any(|p| p.enabled);
        ui.horizontal(|ui| {
            if theme::primary_enabled(ui, any_enabled, "↔  Alle synchronisieren").clicked() {
                self.start_plan(&ctx, Mode::TwoWay, None);
            }
            if theme::secondary_enabled(ui, any_enabled, "🔍  Alle vergleichen").clicked() {
                self.start_plan(&ctx, Mode::Diff, None);
            }
        });
        ui.add_space(6.0);
        let mut action: Option<(Mode, String)> = None;
        let mut toggled = false;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for pair in self.cfg.pairs.iter_mut() {
                theme::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(theme::bold(pair.label(), 17.0));
                        if !pair.enabled {
                            theme::badge(ui, "pausiert", p.muted);
                        }
                        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::toggle(ui, &mut pair.enabled)
                                .on_hover_text("Aktiv: wird bei „Alle …“ und beim Auto-Sync berücksichtigt")
                                .changed()
                            {
                                toggled = true;
                            }
                        });
                    });
                    theme::path_line(ui, "💻", &pair.local);
                    theme::path_line(ui, "☁", &pair.remote);
                    let (t, r) = engine::last_status(pair);
                    match t {
                        Some(t) => ui.label(RichText::new(format!("🕓  Zuletzt {t}  ·  {r}")).size(13.0)),
                        None => ui.label(RichText::new("🕓  Noch nie synchronisiert").size(13.0).color(p.orange)),
                    };
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        if theme::primary(ui, "↔  Synchronisieren").on_hover_text("2-Wege-Sync: Änderungen von beiden Seiten übernehmen").clicked() {
                            action = Some((Mode::TwoWay, pair.id.clone()));
                        }
                        if theme::secondary(ui, "🔍  Vergleichen").on_hover_text("Nur Unterschiede anzeigen, nichts ändern").clicked() {
                            action = Some((Mode::Diff, pair.id.clone()));
                        }
                        if theme::secondary(ui, "⬇  Cloud ➡ Rechner")
                            .on_hover_text("1-Weg: dieser Rechner wird zum Spiegel der Cloud")
                            .clicked()
                        {
                            action = Some((Mode::Pull, pair.id.clone()));
                        }
                        if theme::secondary(ui, "⬆  Rechner ➡ Cloud")
                            .on_hover_text("1-Weg: die Cloud wird zum Spiegel dieses Rechners")
                            .clicked()
                        {
                            action = Some((Mode::Push, pair.id.clone()));
                        }
                    });
                });
                ui.add_space(4.0);
            }
        });
        if toggled {
            self.save();
        }
        if let Some((m, id)) = action {
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
                ui.label(theme::muted(ui, format!("{total} Unterschiede · es wurde nichts verändert")));
            } else {
                ui.label(theme::bold(format!("{sel} von {total} Änderungen ausgewählt"), 14.5));
                if destr > 0 {
                    theme::badge(ui, &format!("{destr} löschen/ersetzen"), p.red);
                }
            }
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                if mode == Mode::Diff {
                    if theme::primary_enabled(ui, total > 0, "↔  Jetzt synchronisieren …").clicked() {
                        self.start_plan(&ctx, Mode::TwoWay, Some(ids.clone()));
                    }
                    if theme::secondary(ui, "Schließen").clicked() {
                        self.job = Job::None;
                    }
                } else {
                    let label = if sel == 0 { "Ausführen".to_string() } else { format!("▶  {sel} Änderungen ausführen") };
                    if theme::primary_enabled(ui, sel > 0, &label).clicked() {
                        if destr > 0 {
                            self.confirm = true;
                        } else {
                            self.start_execute(&ctx);
                        }
                    }
                    if theme::secondary(ui, "⟳  Neu prüfen").clicked() {
                        self.start_plan(&ctx, mode, Some(ids.clone()));
                    }
                    if theme::link(ui, "Abbrechen").clicked() {
                        self.job = Job::None;
                    }
                }
            });
        });
        ui.add_space(10.0);
    }

    fn review(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let Job::Review(mode, plans) = &mut self.job else { return };
        let mode = *mode;
        let sub = if mode == Mode::Diff {
            "Nur eine Ansicht: es wird nichts verändert."
        } else {
            "Noch wurde nichts verändert. Prüfe die Liste und klicke dann unten auf „Ausführen“."
        };
        theme::page_header(ui, &format!("Vorschau: {}", mode_title(mode)), sub);

        // Summary chips
        ui.horizontal_wrapped(|ui| {
            for (a, title, _) in GROUPS {
                let n: usize = plans.iter().filter_map(|x| x.as_ref().ok()).map(|pl| pl.count(|i| i.action == a)).sum();
                if n > 0 {
                    theme::badge(ui, &format!("{n}  {title}"), action_color(&p, a));
                }
            }
            let same: usize = plans.iter().filter_map(|x| x.as_ref().ok()).map(|pl| pl.in_sync.len()).sum();
            theme::badge(ui, &format!("{same}  bereits gleich"), p.muted);
        });
        ui.add_space(4.0);
        ui.add(
            egui::TextEdit::singleline(&mut self.filter)
                .hint_text("🔍  Dateien filtern …")
                .desired_width(320.0)
                .margin(Margin::symmetric(10, 6)),
        );
        let filter = self.filter.to_lowercase();
        ui.add_space(4.0);

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (pi, plan) in plans.iter_mut().enumerate() {
                let plan = match plan {
                    Ok(pl) => pl,
                    Err(e) => {
                        theme::callout(ui, p.red, |ui| {
                            ui.label(theme::bold("Vergleich fehlgeschlagen", 15.0).color(p.red));
                            ui.label(e.as_str());
                        });
                        ui.add_space(6.0);
                        continue;
                    }
                };
                theme::card(ui, |ui| {
                    ui.label(theme::bold(plan.pair.label(), 17.0));
                    theme::path_line(ui, "💻", &format!("{}   ·  {} Dateien", plan.pair.local, plan.local_files));
                    theme::path_line(ui, "☁", &format!("{}   ·  {} Dateien", plan.pair.remote, plan.remote_files));
                    for w in &plan.warnings {
                        theme::callout(ui, p.orange, |ui| {
                            ui.label(RichText::new(format!("⚠  {w}")).color(p.orange));
                        });
                    }
                    if plan.items.is_empty() {
                        ui.add_space(4.0);
                        ui.label(theme::bold("✔  Alles synchron, nichts zu tun.", 15.0).color(p.green));
                        return;
                    }
                    for (a, title, desc) in GROUPS {
                        let idx: Vec<usize> = plan
                            .items
                            .iter()
                            .enumerate()
                            .filter(|(_, it)| it.action == a && (filter.is_empty() || it.path.to_lowercase().contains(&filter)))
                            .map(|(i, _)| i)
                            .collect();
                        if idx.is_empty() {
                            continue;
                        }
                        ui.add_space(10.0);
                        let color = action_color(&p, a);
                        ui.horizontal(|ui| {
                            if mode != Mode::Diff {
                                let n_sel = idx.iter().filter(|&&i| plan.items[i].selected).count();
                                let mut all = n_sel == idx.len();
                                let cb = egui::Checkbox::without_text(&mut all).indeterminate(n_sel > 0 && n_sel < idx.len());
                                if ui.add(cb).changed() {
                                    for &i in &idx {
                                        plan.items[i].selected = all;
                                    }
                                }
                            }
                            let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                            ui.painter().circle_filled(r.center(), 5.0, color);
                            ui.label(theme::bold(title, 15.0));
                            ui.label(theme::muted(ui, format!("{}", idx.len())));
                            if !desc.is_empty() {
                                ui.label(theme::muted(ui, format!("·  {desc}")).small());
                            }
                        });
                        const MAX: usize = 300;
                        egui::Frame::new()
                            .fill(p.surface2)
                            .corner_radius(8)
                            .inner_margin(Margin::symmetric(10, 4))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                for &i in idx.iter().take(MAX) {
                                    file_row(ui, &p, &mut plan.items[i], mode != Mode::Diff);
                                }
                                if idx.len() > MAX {
                                    ui.label(theme::muted(ui, format!("… und {} weitere (Filter nutzen)", idx.len() - MAX)));
                                }
                            });
                    }
                });
                let _ = pi;
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
            ui.label(theme::bold("Wirklich löschen bzw. ersetzen?", 21.0));
            ui.add_space(6.0);
            for (a, what) in [
                (Action::DeleteLocal, "auf diesem Rechner löschen (➡ Papierkorb)"),
                (Action::OverwriteLocal, "auf diesem Rechner ersetzen (alte Version ➡ Papierkorb)"),
                (Action::DeleteRemote, "in der Cloud löschen"),
                (Action::OverwriteRemote, "in der Cloud ersetzen"),
            ] {
                let n = items.iter().filter(|i| i.1 == a).count();
                if n > 0 {
                    ui.horizontal(|ui| {
                        theme::badge(ui, &n.to_string(), action_color(&p, a));
                        ui.label(format!("Dateien {what}"));
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
                "In der Cloud gelöschte oder ersetzte Dateien landen im Backup-Ordner des Ordnerpaars (falls gesetzt), \
                 sonst im Papierkorb bzw. in den Versionen des Anbieters.",
            ).small());
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::danger(ui, "Ja, ausführen").clicked() {
                    go = Some(true);
                }
                if theme::secondary(ui, "Abbrechen").clicked() {
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
                        ("⚠", p.orange, "Fertig, aber nicht alles hat geklappt")
                    } else {
                        ("✔", p.green, "Fertig")
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
                    ui.label(theme::bold(format!("⚠  {conflicts} Konflikt(e): beide Versionen wurden behalten"), 15.0).color(p.orange));
                    for r in results.iter().filter_map(|r| r.as_ref().ok()) {
                        for (orig, l, rr) in &r.1.conflicts {
                            ui.label(RichText::new(format!("{orig}\n    ➡ {l}\n    ➡ {rr}")).size(13.0));
                        }
                    }
                    ui.add_space(4.0);
                    ui.label(
                        "So löst du einen Konflikt: Öffne beide Dateien, benenne die richtige wieder in den \
                         Originalnamen um und lösche die andere. Beim nächsten Sync wird das übernommen.",
                    );
                });
            }
            ui.add_space(12.0);
            if theme::primary(ui, "Fertig").clicked() {
                close = true;
            }
        });
        if close {
            self.job = Job::None;
        }
    }

    // ------------------------------------------------------------------ pairs

    fn pairs_page(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        theme::page_header(ui, "Ordnerpaare", "Welcher Ordner auf diesem Rechner gehört zu welchem Ordner in der Cloud");
        if theme::primary(ui, "➕  Ordnerpaar hinzufügen").clicked() {
            self.wizard = Some(Wizard::new(Step::Connection, false));
            return;
        }
        ui.add_space(6.0);
        let remotes: Vec<String> = match &self.rinfo {
            Some(Ok((_, r))) => r.iter().map(|x| x.0.clone()).collect(),
            _ => Vec::new(),
        };
        let mut save_edit = false;
        let mut cancel_edit = false;
        let mut start_edit = None;
        let mut reset = None;
        let mut toggled = false;
        let cfg_snapshot = self.cfg.clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (i, pair) in self.cfg.pairs.iter_mut().enumerate() {
                theme::card(ui, |ui| {
                    if let Some(ed) = self.edit.as_mut().filter(|e| e.index == i) {
                        let d = &mut ed.draft;
                        ui.label(theme::bold("Ordnerpaar bearbeiten", 17.0));
                        ui.add_space(4.0);
                        field(ui, "Name");
                        ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(f32::INFINITY));
                        field(ui, "💻  Ordner auf diesem Rechner");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut d.local).desired_width(ui.available_width() - 130.0));
                            if theme::secondary(ui, "📂  Wählen …").clicked()
                                && let Some(f) = rfd::FileDialog::new().pick_folder() {
                                    d.local = f.display().to_string();
                                }
                        });
                        field(ui, "☁  Ordner in der Cloud");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut d.remote).desired_width(ui.available_width() - 150.0));
                            if theme::secondary(ui, if ed.browse { "Fertig" } else { "☁  Durchsuchen …" }).clicked() {
                                ed.browse = !ed.browse;
                                if ed.browse {
                                    let (r, path) = d.remote.split_once(':').unwrap_or((remotes.first().map(String::as_str).unwrap_or(""), ""));
                                    ed.browser = RemoteBrowser::new(r, path);
                                }
                            }
                        });
                        if ed.browse {
                            ed.browser.ui(ui, &cfg_snapshot, 180.0);
                            d.remote = ed.browser.full();
                        }
                        egui::CollapsingHeader::new("Erweitert").default_open(false).show(ui, |ui| {
                            field(ui, "Backup-Ordner in der Cloud (optional)");
                            ui.add(
                                egui::TextEdit::singleline(&mut d.remote_backup)
                                    .desired_width(f32::INFINITY)
                                    .hint_text("z. B. gdrive:_draisine-backup  (leer = Papierkorb/Versionen des Anbieters)"),
                            );
                            ui.label(theme::muted(ui, "Muss auf derselben Verbindung, aber außerhalb des synchronisierten Ordners liegen.").small());
                            field(ui, "Ausschließen (ein Muster pro Zeile)");
                            let mut s = d.excludes.join("\n");
                            if ui.add(egui::TextEdit::multiline(&mut s).desired_rows(3).desired_width(f32::INFINITY).hint_text("z. B. 2020/GEHALT/**\n*.tmp")).changed() {
                                d.excludes = s.split('\n').map(str::to_string).collect();
                            }
                        });
                        if d.signature() != pair.signature() {
                            theme::callout(ui, p.blue, |ui| {
                                ui.label(RichText::new("ℹ  Ordner oder Ausschlüsse geändert: der nächste Sync verhält sich wie ein erster Sync (nichts wird gelöscht).").size(13.0));
                            });
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            let ok = !d.local.trim().is_empty() && d.remote.contains(':');
                            if theme::primary_enabled(ui, ok, "Speichern").clicked() {
                                save_edit = true;
                            }
                            if theme::secondary(ui, "Abbrechen").clicked() {
                                cancel_edit = true;
                            }
                        });
                        return;
                    }
                    ui.horizontal(|ui| {
                        ui.label(theme::bold(pair.label(), 17.0));
                        if !pair.enabled {
                            theme::badge(ui, "pausiert", p.muted);
                        }
                        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                            toggled |= theme::toggle(ui, &mut pair.enabled).on_hover_text("aktiv / pausiert").changed();
                        });
                    });
                    theme::path_line(ui, "💻", &pair.local);
                    theme::path_line(ui, "☁", &pair.remote);
                    if !pair.excludes.is_empty() {
                        ui.label(theme::muted(ui, format!("Ausgeschlossen: {}", pair.excludes.join(", "))).small());
                    }
                    if !pair.remote_backup.is_empty() {
                        ui.label(theme::muted(ui, format!("Backup: {}", pair.remote_backup)).small());
                    }
                    ui.horizontal(|ui| {
                        if theme::secondary(ui, "✏  Bearbeiten").clicked() {
                            start_edit = Some(i);
                        }
                        if theme::secondary(ui, "🗑  Entfernen").clicked() {
                            self.delete_pair = Some(i);
                        }
                        if theme::link(ui, "Sync-Status zurücksetzen")
                            .on_hover_text("Vergisst den letzten Sync. Der nächste Sync verhält sich wie der erste: nichts wird gelöscht.")
                            .clicked()
                        {
                            reset = Some(i);
                        }
                    });
                });
                ui.add_space(4.0);
            }
        });
        if toggled {
            self.save();
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
                log::info(format!("Ordnerpaar geändert: {}", ed.draft.label()));
                self.cfg.pairs[ed.index] = ed.draft;
                self.save();
                self.flash("Gespeichert");
            }
        if let Some(i) = reset {
            engine::reset_state(&self.cfg.pairs[i]);
            self.flash("Sync-Status zurückgesetzt");
        }
    }

    fn delete_modal(&mut self, ctx: &egui::Context) {
        let Some(i) = self.delete_pair else { return };
        let Some(pair) = self.cfg.pairs.get(i).cloned() else {
            self.delete_pair = None;
            return;
        };
        let mut go = None;
        egui::Modal::new(egui::Id::unique("draisine-delete-pair")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(theme::bold(format!("„{}“ entfernen?", pair.label()), 20.0));
            ui.add_space(4.0);
            ui.label("Das Ordnerpaar wird nur aus Draisine entfernt. Es werden keine Dateien gelöscht, weder auf diesem Rechner noch in der Cloud.");
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::danger(ui, "Entfernen").clicked() {
                    go = Some(true);
                }
                if theme::secondary(ui, "Abbrechen").clicked() {
                    go = Some(false);
                }
            });
        });
        match go {
            Some(true) => {
                log::info(format!("Ordnerpaar entfernt: {}", pair.label()));
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
        theme::page_header(ui, "Verbindungen", "Cloud-Speicher, mit denen Draisine Dateien austauschen kann");
        ui.horizontal(|ui| {
            if theme::primary(ui, "➕  Neue Verbindung").clicked() {
                self.wizard = Some(Wizard::new(Step::Connection, true));
            }
            if theme::secondary(ui, "⟳  Aktualisieren").clicked() {
                self.load_rinfo(ui.ctx());
            }
        });
        ui.add_space(6.0);
        match &self.rinfo {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Lade …");
                });
            }
            Some(Err(e)) => {
                theme::callout(ui, p.red, |ui| {
                    ui.label(theme::bold("rclone nicht gefunden", 15.0).color(p.red));
                    ui.label(e.as_str());
                });
            }
            Some(Ok((_, remotes))) => {
                if remotes.is_empty() {
                    theme::card(ui, |ui| {
                        ui.label(theme::bold("Noch keine Verbindung", 16.0));
                        ui.label(theme::muted(ui, "Lege mit „Neue Verbindung“ eine an, z. B. zu Google Drive."));
                    });
                }
                let remotes = remotes.clone();
                for (name, typ) in remotes {
                    let used = self.cfg.pairs.iter().filter(|pp| pp.remote.split(':').next() == Some(name.as_str())).count();
                    theme::card(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("☁").size(22.0).color(p.accent));
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(theme::bold(&name, 16.0));
                                    theme::badge(ui, &typ, p.blue);
                                });
                                ui.label(theme::muted(ui, if used == 0 { "von keinem Ordnerpaar verwendet".to_string() } else { format!("von {used} Ordnerpaar(en) verwendet") }));
                            });
                            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                                if self.test_jobs.contains_key(&name) {
                                    ui.spinner();
                                } else if theme::secondary(ui, "Verbindung testen").clicked() {
                                    let (cfg, n) = (self.cfg.clone(), name.clone());
                                    self.tests.remove(&name);
                                    self.test_jobs.insert(
                                        name.clone(),
                                        Bg::spawn(ui.ctx(), move || Rclone::new(&cfg).map_err(|e| e.to_string())?.test_remote(&n).map_err(|e| e.to_string())),
                                    );
                                }
                                match self.tests.get(&name) {
                                    Some(Ok(())) => {
                                        theme::badge(ui, "✔ erreichbar", p.green);
                                    }
                                    Some(Err(e)) => {
                                        theme::badge(ui, "✖ Fehler", p.red).on_hover_text(e);
                                    }
                                    None => {}
                                }
                            });
                        });
                    });
                    ui.add_space(4.0);
                }
                ui.add_space(6.0);
                ui.label(theme::muted(ui, "Verbindungen ändern oder löschen: im Terminal „rclone config“.").small());
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
        theme::page_header(ui, "Protokoll", "Alles, was Draisine tut. Eine Textdatei pro Tag, nichts wird gelöscht.");
        ui.horizontal(|ui| {
            if theme::secondary(ui, "📄  Als TXT öffnen").clicked() {
                log::open_today();
            }
            if theme::secondary(ui, "📁  Ordner öffnen").clicked() {
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
        theme::page_header(ui, "Einstellungen", "Änderungen werden sofort gespeichert");
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    theme::toggle(ui, &mut self.cfg.auto_sync);
                    ui.label(theme::bold("Automatisch synchronisieren", 16.0));
                });
                ui.horizontal(|ui| {
                    ui.label("Alle");
                    ui.add(egui::DragValue::new(&mut self.cfg.interval_minutes).range(1..=1440));
                    ui.label("Minuten, solange Draisine im Tray läuft.");
                });
                ui.label(theme::muted(ui, "Neue Dateien und Konflikte werden automatisch übernommen. Löschen und Ersetzen wartet immer auf deine Bestätigung: das Tray-Symbol wird dann orange."));
            });
            ui.add_space(6.0);
            theme::card(ui, |ui| {
                ui.label(theme::bold("Sicherheit", 16.0));
                ui.horizontal(|ui| {
                    ui.label("Warnen, wenn mehr als");
                    ui.add(egui::DragValue::new(&mut self.cfg.max_delete_percent).range(1..=100).suffix(" %"));
                    ui.label("der Dateien gelöscht würden");
                });
                ui.horizontal(|ui| {
                    theme::toggle(ui, &mut self.cfg.check_download);
                    ui.label("Inhalte notfalls per Download vergleichen (genauer, aber langsam)");
                });
            });
            ui.add_space(6.0);
            theme::card(ui, |ui| {
                ui.label(theme::bold("rclone", 16.0));
                match &self.rinfo {
                    Some(Ok((v, _))) => ui.label(RichText::new(format!("✔  {v}")).color(p.green)),
                    Some(Err(e)) => ui.label(RichText::new(format!("✖  {e}")).color(p.red)),
                    None => ui.label(theme::muted(ui, "prüfe …")),
                };
                egui::CollapsingHeader::new("Erweitert").default_open(false).show(ui, |ui| {
                    field(ui, "Programm (leer = neben Draisine, sonst PATH)");
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.rclone_path).desired_width(f32::INFINITY));
                    field(ui, "rclone.conf (leer = neben Draisine, sonst rclone-Standard)");
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.rclone_config).desired_width(f32::INFINITY));
                    field(ui, "Zusätzliche Flags (eins pro Zeile)");
                    let mut s = self.cfg.extra_flags.join("\n");
                    if ui.add(egui::TextEdit::multiline(&mut s).desired_rows(2).desired_width(f32::INFINITY)).changed() {
                        self.cfg.extra_flags = s.split('\n').map(str::to_string).collect();
                    }
                    if theme::secondary(ui, "⟳  Erneut prüfen").clicked() {
                        self.load_rinfo(ui.ctx());
                    }
                });
            });
            ui.add_space(6.0);
            theme::card(ui, |ui| {
                ui.label(theme::bold("Speicherorte", 16.0));
                let d = dirs();
                for (label, path) in [
                    ("Einstellungen", d.config_file.parent().unwrap().to_path_buf()),
                    ("Sync-Status", d.state_dir.clone()),
                    ("Protokolle", d.log_dir.clone()),
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
                        "Portabler Modus: alles liegt im Ordner „draisine-data“ neben dem Programm.".to_string()
                    } else {
                        format!("Tipp: Ein Ordner „{}“ neben dem Programm schaltet den portablen Modus ein.", crate::config::PORTABLE_DIR)
                    },
                ).small());
            });
            ui.add_space(6.0);
            theme::card(ui, |ui| {
                ui.label(theme::bold("Einrichtung", 16.0));
                if theme::secondary(ui, "✨  Einrichtungsassistent erneut starten").clicked() {
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

fn field(ui: &mut Ui, label: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(label).size(13.0).color(pal(ui).muted));
}

fn file_row(ui: &mut Ui, p: &Pal, it: &mut engine::Item, selectable: bool) {
    ui.horizontal(|ui| {
        ui.set_min_height(28.0);
        if selectable {
            ui.add(egui::Checkbox::without_text(&mut it.selected));
        }
        let (dir, file) = match it.path.rfind('/') {
            Some(i) => (&it.path[..=i], &it.path[i + 1..]),
            None => ("", it.path.as_str()),
        };
        let mut job = egui::text::LayoutJob::default();
        let f = egui::FontId::proportional(14.0);
        job.append(dir, 0.0, egui::TextFormat::simple(f.clone(), p.muted));
        job.append(file, 0.0, egui::TextFormat::simple(f, p.text));
        let resp = ui.add(egui::Label::new(job).truncate());
        if !it.reason.is_empty() {
            resp.on_hover_text(&it.reason);
        }
        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
            let meta = |e: &Option<Entry>| e.as_ref().map(|e| format!("{}  {}", human(e.size), when(e))).unwrap_or_else(|| "–".into());
            ui.label(RichText::new(format!("☁ {}", meta(&it.remote))).size(12.0).color(p.muted));
            ui.label(RichText::new(format!("💻 {}", meta(&it.local))).size(12.0).color(p.muted));
            if !it.reason.is_empty() {
                ui.label(RichText::new(&it.reason).size(12.0).color(p.muted.gamma_multiply(0.8)));
            }
        });
    });
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
                Page::Pairs => self.pairs_page(ui),
                Page::Connections => self.connections_page(ui),
                Page::Log => self.log_page(ui),
                Page::Settings => self.settings_page(ui),
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
