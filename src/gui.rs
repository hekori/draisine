//! Window process (started by the tray with `--window`): settings, previews, confirmations, logs.

use crate::config::{APP_NAME, Config, Pair, dirs};
use crate::engine::{self, Action, Mode, Plan, Report};
use crate::rclone::{Entry, Rclone};
use crate::{icon, ipc, log};
use eframe::egui::{self, Color32, RichText};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

enum Msg {
    Plans(Mode, Vec<Result<Plan, String>>),
    Progress(String),
    Executed(Vec<Result<(String, Report), String>>),
    RcloneInfo(String, Vec<String>),
    RemoteSetup(String),
}

enum Job {
    Idle,
    Planning(Mode),
    Review(Mode, Vec<Result<Plan, String>>),
    Executing(Vec<String>),
    Done(Vec<Result<(String, Report), String>>),
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Sync,
    Settings,
    Logs,
}

struct Gui {
    cfg: Config,
    saved: Config,
    tab: Tab,
    job: Job,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
    confirm: bool,
    only_destructive: bool,
    log_text: String,
    log_loaded: Instant,
    last_poll: Instant,
    rclone_version: String,
    remotes: Vec<String>,
    new_remote_name: String,
    new_remote_type: String,
    remote_setup_out: String,
    remote_setup_running: bool,
    save_msg: String,
    pending_replan: Option<Mode>,
}

const RED: Color32 = Color32::from_rgb(215, 60, 60);
const ORANGE: Color32 = Color32::from_rgb(230, 140, 20);
const GREEN: Color32 = Color32::from_rgb(60, 160, 80);
const BLUE: Color32 = Color32::from_rgb(60, 130, 220);

pub fn run(action: Option<Mode>) -> anyhow::Result<()> {
    let Some(_lock) = ipc::try_lock("window.lock") else {
        // A window is already open; it picks the request up.
        ipc::request_window(action);
        return Ok(());
    };
    let _ = ipc::take_request(); // stale request from before
    let px = icon::rgba(64, icon::Status::Idle.color());
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_app_id("draisine")
            .with_inner_size([1000.0, 700.0])
            .with_icon(egui::IconData { rgba: px, width: 64, height: 64 }),
        ..Default::default()
    };
    eframe::run_native(APP_NAME, opts, Box::new(move |cc| Ok(Box::new(Gui::new(cc, action)))))
        .map_err(|e| anyhow::anyhow!("Fenster: {e}"))
}

fn human(n: i64) -> String {
    let n = n as f64;
    if n < 1024.0 {
        format!("{n} B")
    } else if n < 1024.0 * 1024.0 {
        format!("{:.1} KB", n / 1024.0)
    } else if n < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", n / 1024.0 / 1024.0)
    } else {
        format!("{:.2} GB", n / 1024.0 / 1024.0 / 1024.0)
    }
}

fn entry_text(e: Option<Entry>) -> String {
    match e {
        None => "–".into(),
        Some(e) => {
            let t = chrono::DateTime::from_timestamp_millis(e.mtime_ms)
                .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();
            format!("{}  {t}", human(e.size))
        }
    }
}

fn action_color(a: Action) -> Color32 {
    match a {
        Action::DeleteLocal | Action::DeleteRemote => RED,
        Action::OverwriteLocal | Action::OverwriteRemote | Action::Conflict | Action::Different => ORANGE,
        Action::CopyToLocal | Action::CopyToRemote => GREEN,
        Action::OnlyLocal | Action::OnlyRemote => BLUE,
    }
}

impl Gui {
    fn new(cc: &eframe::CreationContext, action: Option<Mode>) -> Gui {
        let (tx, rx) = mpsc::channel();
        let cfg = Config::load();
        let mut g = Gui {
            saved: cfg.clone(),
            cfg,
            tab: Tab::Sync,
            job: Job::Idle,
            tx,
            rx,
            ctx: cc.egui_ctx.clone(),
            confirm: false,
            only_destructive: false,
            log_text: String::new(),
            log_loaded: Instant::now() - Duration::from_secs(60),
            last_poll: Instant::now(),
            rclone_version: "…".into(),
            remotes: Vec::new(),
            new_remote_name: "gdrive".into(),
            new_remote_type: "drive".into(),
            remote_setup_out: String::new(),
            remote_setup_running: false,
            save_msg: String::new(),
            pending_replan: None,
        };
        g.load_rclone_info();
        if let Some(m) = action {
            g.start_plan(m);
        }
        g
    }

    fn busy(&self) -> bool {
        matches!(self.job, Job::Planning(_) | Job::Executing(_))
    }

    fn send(tx: &Sender<Msg>, ctx: &egui::Context, m: Msg) {
        let _ = tx.send(m);
        ctx.request_repaint();
    }

    fn load_rclone_info(&mut self) {
        let (tx, ctx, cfg) = (self.tx.clone(), self.ctx.clone(), self.saved.clone());
        std::thread::spawn(move || {
            let msg = match Rclone::new(&cfg) {
                Ok(rc) => Msg::RcloneInfo(format!("{} ({})", rc.version(), rc.bin.display()), rc.listremotes()),
                Err(e) => Msg::RcloneInfo(format!("⚠ {e}"), Vec::new()),
            };
            Self::send(&tx, &ctx, msg);
        });
    }

    fn start_plan(&mut self, mode: Mode) {
        if self.busy() {
            return;
        }
        if self.cfg != self.saved {
            self.save();
        }
        let pairs: Vec<Pair> = self.saved.pairs.iter().filter(|p| p.enabled).cloned().collect();
        if pairs.is_empty() {
            self.job = Job::Review(mode, vec![Err("Kein aktives Ordnerpaar. Bitte unter „Einstellungen“ anlegen.".into())]);
            return;
        }
        self.tab = Tab::Sync;
        self.job = Job::Planning(mode);
        self.confirm = false;
        log::info(format!("Vorschau: {}", mode.label()));
        let (tx, ctx, cfg) = (self.tx.clone(), self.ctx.clone(), self.saved.clone());
        std::thread::spawn(move || {
            let plans = pairs
                .iter()
                .map(|p| {
                    engine::plan(&cfg, p, mode).map_err(|e| {
                        log::error(format!("[{}] Vorschau fehlgeschlagen: {e:#}", p.label()));
                        format!("{}: {e:#}", p.label())
                    })
                })
                .collect();
            Self::send(&tx, &ctx, Msg::Plans(mode, plans));
        });
    }

    fn start_execute(&mut self) {
        let Job::Review(_, plans) = &self.job else { return };
        let plans: Vec<Plan> = plans.iter().filter_map(|p| p.as_ref().ok().cloned()).collect();
        self.job = Job::Executing(vec!["Starte …".into()]);
        self.confirm = false;
        let (tx, ctx, cfg) = (self.tx.clone(), self.ctx.clone(), self.saved.clone());
        std::thread::spawn(move || {
            let mut results = Vec::new();
            for p in &plans {
                let label = p.pair.label();
                Self::send(&tx, &ctx, Msg::Progress(format!("── {label}")));
                let r = engine::execute(&cfg, p, &mut |l| Self::send(&tx, &ctx, Msg::Progress(l.to_string())));
                results.push(match r {
                    Ok(rep) => Ok((label, rep)),
                    Err(e) => {
                        log::error(format!("[{label}] {e:#}"));
                        Err(format!("{label}: {e:#}"))
                    }
                });
            }
            Self::send(&tx, &ctx, Msg::Executed(results));
        });
    }

    fn save(&mut self) {
        for p in self.cfg.pairs.iter_mut() {
            p.excludes = p.excludes.iter().map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).collect();
            p.local = p.local.trim().to_string();
            p.remote = p.remote.trim().to_string();
            p.remote_backup = p.remote_backup.trim().to_string();
        }
        self.cfg.extra_flags.retain(|f| !f.trim().is_empty());
        match self.cfg.save() {
            Ok(()) => {
                self.saved = self.cfg.clone();
                self.save_msg = "Gespeichert.".into();
                log::info("Einstellungen gespeichert");
            }
            Err(e) => self.save_msg = format!("Speichern fehlgeschlagen: {e}"),
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Plans(mode, plans) => self.job = Job::Review(mode, plans),
                Msg::Progress(l) => {
                    if let Job::Executing(lines) = &mut self.job {
                        lines.push(l);
                        if lines.len() > 400 {
                            lines.drain(..100);
                        }
                    }
                }
                Msg::Executed(r) => self.job = Job::Done(r),
                Msg::RcloneInfo(v, r) => {
                    self.rclone_version = v;
                    self.remotes = r;
                }
                Msg::RemoteSetup(out) => {
                    self.remote_setup_out = out;
                    self.remote_setup_running = false;
                    self.load_rclone_info();
                }
            }
        }
        if self.last_poll.elapsed() > Duration::from_millis(500) {
            self.last_poll = Instant::now();
            if let Some(req) = ipc::take_request() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                if let Some(m) = req {
                    self.start_plan(m);
                }
            }
            // Auto-sync may be toggled from the tray.
            let disk = Config::load();
            if disk.auto_sync != self.saved.auto_sync {
                self.saved.auto_sync = disk.auto_sync;
                self.cfg.auto_sync = disk.auto_sync;
            }
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    // ------------------------------------------------------------ sync tab

    fn sync_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Ordnerpaare");
        if self.saved.pairs.is_empty() {
            ui.label("Noch keine Ordnerpaare. Lege unter „Einstellungen“ eines an.");
        } else {
            egui::Grid::new("pairs").striped(true).num_columns(5).spacing([16.0, 4.0]).show(ui, |ui| {
                ui.strong("");
                ui.strong("Name");
                ui.strong("Lokal ↔ Remote");
                ui.strong("Letzter Sync");
                ui.end_row();
                let mut changed = false;
                for p in self.cfg.pairs.iter_mut() {
                    changed |= ui.checkbox(&mut p.enabled, "").on_hover_text("bei Aktionen einbeziehen").changed();
                    ui.label(p.label());
                    ui.label(format!("{}  ↔  {}", p.local, p.remote));
                    let (t, r) = engine::last_status(p);
                    ui.label(t.map(|t| format!("{t}  {r}")).unwrap_or_else(|| "noch nie".into()));
                    ui.end_row();
                }
                if changed {
                    for (s, c) in self.saved.pairs.iter_mut().zip(self.cfg.pairs.iter()) {
                        s.enabled = c.enabled;
                    }
                    let mut disk = Config::load();
                    for (d, c) in disk.pairs.iter_mut().zip(self.cfg.pairs.iter()) {
                        if d.id == c.id {
                            d.enabled = c.enabled;
                        }
                    }
                    let _ = disk.save();
                }
            });
        }
        ui.add_space(8.0);
        let enabled = !self.busy();
        ui.horizontal_wrapped(|ui| {
            for m in [Mode::Diff, Mode::Pull, Mode::Push, Mode::TwoWay] {
                let icon = match m {
                    Mode::Diff => "🔍",
                    Mode::Pull => "⬇",
                    Mode::Push => "⬆",
                    Mode::TwoWay => "↔",
                };
                if ui.add_enabled(enabled, egui::Button::new(format!("{icon} {}", m.label()))).clicked() {
                    self.start_plan(m);
                }
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let mut auto = self.cfg.auto_sync;
            if ui.checkbox(&mut auto, "Auto-Sync").changed() {
                self.cfg.auto_sync = auto;
                self.saved.auto_sync = auto;
                let mut disk = Config::load();
                disk.auto_sync = auto;
                let _ = disk.save();
                log::info(format!("Auto-Sync {}", if auto { "an" } else { "aus" }));
            }
            ui.label(
                RichText::new(format!(
                    "2-Wege-Sync alle {} min im Hintergrund (Tray). Neue Dateien und Konflikte automatisch; \
                     Löschen und Überschreiben wartet immer auf deine Bestätigung.",
                    self.saved.interval_minutes
                ))
                .weak(),
            );
        });
        ui.separator();

        match &mut self.job {
            Job::Idle => {}
            Job::Planning(m) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("{}: vergleiche lokal und remote …", m.label()));
                });
            }
            Job::Executing(lines) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Wird ausgeführt …");
                });
                egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                    for l in lines.iter() {
                        ui.monospace(l);
                    }
                });
            }
            Job::Done(results) => {
                ui.heading("Fertig");
                for r in results.iter() {
                    match r {
                        Ok((label, rep)) => {
                            ui.label(RichText::new(format!("✔ {label}: {}", rep.summary())).color(GREEN));
                            for (orig, l, r) in &rep.conflicts {
                                ui.label(RichText::new(format!("   ⚠ Konflikt {orig} ➡ {l}, {r}")).color(ORANGE));
                            }
                            for f in &rep.failed {
                                ui.label(RichText::new(format!("   ✖ {f}")).color(RED));
                            }
                        }
                        Err(e) => {
                            ui.label(RichText::new(format!("✖ {e}")).color(RED));
                        }
                    }
                }
                ui.add_space(6.0);
                if ui.button("OK").clicked() {
                    self.job = Job::Idle;
                }
            }
            Job::Review(..) => self.review(ui),
        }
    }

    fn review(&mut self, ui: &mut egui::Ui) {
        let only_destructive = &mut self.only_destructive;
        let Job::Review(mode, plans) = &mut self.job else { return };
        let mode = *mode;
        ui.horizontal(|ui| {
            ui.heading(format!("Vorschau: {}", mode.label()));
            ui.checkbox(only_destructive, "nur Löschen/Überschreiben/Konflikte zeigen");
        });
        if mode != Mode::Diff {
            ui.label(
                RichText::new(
                    "Noch wurde nichts geändert. Abgehakte Aktionen werden mit „Ausführen“ durchgeführt. \
                     Lokal gelöschte oder überschriebene Dateien landen im Papierkorb.",
                )
                .weak(),
            );
        }
        let mut close = false;
        let mut run = false;
        let mut sel = 0;
        let mut destructive = 0;
        egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - 40.0).show(ui, |ui| {
            for (pi, p) in plans.iter_mut().enumerate() {
                let p = match p {
                    Ok(p) => p,
                    Err(e) => {
                        ui.label(RichText::new(format!("✖ {e}")).color(RED));
                        continue;
                    }
                };
                let n = |a: &[Action]| p.count(|i| a.contains(&i.action));
                let head = if mode == Mode::Diff {
                    format!(
                        "{}: {} nur lokal, {} nur remote, {} unterschiedlich, {} gleich",
                        p.pair.label(),
                        n(&[Action::OnlyLocal]),
                        n(&[Action::OnlyRemote]),
                        n(&[Action::Different]),
                        p.in_sync.len()
                    )
                } else {
                    format!(
                        "{}: {} Aktionen ({} kopieren, {} überschreiben, {} löschen, {} Konflikte), {} bereits gleich",
                        p.pair.label(),
                        p.items.len(),
                        n(&[Action::CopyToLocal, Action::CopyToRemote]),
                        n(&[Action::OverwriteLocal, Action::OverwriteRemote]),
                        n(&[Action::DeleteLocal, Action::DeleteRemote]),
                        n(&[Action::Conflict]),
                        p.in_sync.len()
                    )
                };
                egui::CollapsingHeader::new(RichText::new(head).strong()).id_salt(("plan", pi)).default_open(true).show(
                    ui,
                    |ui| {
                        ui.label(
                            RichText::new(format!(
                                "{}  ({} Dateien)   ↔   {}  ({} Dateien)",
                                p.pair.local, p.local_files, p.pair.remote, p.remote_files
                            ))
                            .weak(),
                        );
                        for w in &p.warnings {
                            ui.label(RichText::new(format!("⚠ {w}")).color(ORANGE));
                        }
                        if p.items.is_empty() {
                            ui.label(RichText::new("✔ Alles synchron, nichts zu tun.").color(GREEN));
                            return;
                        }
                        if mode != Mode::Diff {
                            ui.horizontal(|ui| {
                                if ui.small_button("alle auswählen").clicked() {
                                    p.items.iter_mut().for_each(|i| i.selected = true);
                                }
                                if ui.small_button("keine").clicked() {
                                    p.items.iter_mut().for_each(|i| i.selected = false);
                                }
                                if ui.small_button("nur sichere (kopieren, Konflikte)").clicked() {
                                    p.items.iter_mut().for_each(|i| i.selected = !i.action.destructive());
                                }
                            });
                        }
                        egui::Grid::new(("items", pi)).striped(true).num_columns(5).spacing([12.0, 2.0]).show(ui, |ui| {
                            ui.strong("Aktion");
                            ui.strong("Pfad");
                            ui.strong("Grund");
                            ui.strong("lokal");
                            ui.strong("remote");
                            ui.end_row();
                            for it in p.items.iter_mut() {
                                let shown = !*only_destructive
                                    || it.action.destructive()
                                    || matches!(it.action, Action::Conflict | Action::Different);
                                if !shown {
                                    continue;
                                }
                                let txt = RichText::new(it.action.label()).color(action_color(it.action));
                                if mode == Mode::Diff {
                                    ui.label(txt);
                                } else {
                                    ui.checkbox(&mut it.selected, txt);
                                }
                                ui.label(&it.path);
                                ui.label(RichText::new(&it.reason).weak());
                                ui.monospace(entry_text(it.local));
                                ui.monospace(entry_text(it.remote));
                                ui.end_row();
                            }
                        });
                    },
                );
                sel += p.count(|i| i.selected && i.action.executable());
                destructive += p.count(|i| i.selected && i.action.destructive());
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            if mode == Mode::Diff {
                if ui.button("Schließen").clicked() {
                    close = true;
                }
                if ui.button("Neu vergleichen").clicked() {
                    close = true;
                    self.pending_replan = Some(mode);
                }
                return;
            }
            let label = format!("▶ Ausführen ({sel} ausgewählt)");
            if ui.add(egui::Button::new(RichText::new(label).strong())).clicked() {
                if destructive > 0 {
                    self.confirm = true;
                } else {
                    run = true;
                }
            }
            if ui.button("Abbrechen").clicked() {
                close = true;
            }
            if ui.button("Neu prüfen").clicked() {
                close = true;
                self.pending_replan = Some(mode);
            }
        });
        if close {
            self.job = Job::Idle;
        }
        if run {
            self.start_execute();
        }
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
            .flat_map(|p| {
                p.items.iter().filter(|i| i.selected && i.action.destructive()).map(|i| (p.pair.label(), i.action, i.path.clone()))
            })
            .collect();
        let c = |a: Action| items.iter().filter(|i| i.1 == a).count();
        let mut go = None;
        egui::Modal::new(egui::Id::unique("draisine-confirm")).show(ctx, |ui| {
            ui.set_max_width(640.0);
            ui.heading("Wirklich löschen / überschreiben?");
            ui.add_space(4.0);
            for (a, what) in [
                (Action::DeleteLocal, "lokal löschen (➡ Papierkorb)"),
                (Action::OverwriteLocal, "lokal überschreiben (alte Version ➡ Papierkorb)"),
                (Action::DeleteRemote, "remote löschen"),
                (Action::OverwriteRemote, "remote überschreiben"),
            ] {
                if c(a) > 0 {
                    ui.label(RichText::new(format!("{} Dateien {what}", c(a))).color(action_color(a)));
                }
            }
            ui.add_space(4.0);
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                for (pair, a, p) in &items {
                    ui.label(RichText::new(format!("{}  [{pair}] {p}", a.label())).color(action_color(*a)).small());
                }
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Remote: gelöschte/überschriebene Dateien gehen in den Backup-Ordner des Paars (falls gesetzt), \
                     sonst in den Papierkorb bzw. die Versionen des Anbieters (Google Drive: Papierkorb).",
                )
                .weak()
                .small(),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Ja, ausführen").color(RED).strong()).clicked() {
                    go = Some(true);
                }
                if ui.button("Abbrechen").clicked() {
                    go = Some(false);
                }
            });
        });
        match go {
            Some(true) => self.start_execute(),
            Some(false) => self.confirm = false,
            None => {}
        }
    }

    // ------------------------------------------------------------ settings tab

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.heading("Ordnerpaare");
            let mut remove = None;
            let remotes = self.remotes.clone();
            for (i, p) in self.cfg.pairs.iter_mut().enumerate() {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    egui::Grid::new(("pair", i)).num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                        ui.label("Name");
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut p.name);
                            ui.checkbox(&mut p.enabled, "aktiv");
                        });
                        ui.end_row();
                        ui.label("Lokaler Ordner");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut p.local).desired_width(480.0));
                            if ui.button("Wählen …").clicked()
                                && let Some(d) = rfd::FileDialog::new().pick_folder() {
                                    p.local = d.display().to_string();
                                }
                        });
                        ui.end_row();
                        ui.label("Remote");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut p.remote).desired_width(480.0).hint_text("gdrive:Ordner"));
                            egui::ComboBox::from_id_salt(("remote", i)).selected_text("Remote …").show_ui(ui, |ui| {
                                for r in &remotes {
                                    if ui.selectable_label(false, r).clicked() {
                                        let rest = p.remote.split_once(':').map(|x| x.1).unwrap_or("").to_string();
                                        p.remote = format!("{r}{rest}");
                                    }
                                }
                            });
                        });
                        ui.end_row();
                        ui.label("Remote-Backup").on_hover_text(
                            "Optional. Remote-Dateien werden vor dem Löschen/Überschreiben hierhin verschoben \
                             (Unterordner mit Zeitstempel). Muss auf demselben Remote liegen, aber außerhalb des \
                             synchronisierten Ordners, z. B. gdrive:_draisine-backup",
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut p.remote_backup)
                                .desired_width(480.0)
                                .hint_text("leer = Papierkorb/Versionen des Anbieters"),
                        );
                        ui.end_row();
                        ui.label("Ausschließen").on_hover_text("rclone-Filtermuster, eins pro Zeile, z. B. 2020/GEHALT/**");
                        let mut s = p.excludes.join("\n");
                        if ui.add(egui::TextEdit::multiline(&mut s).desired_rows(2).desired_width(480.0)).changed() {
                            p.excludes = s.split('\n').map(str::to_string).collect();
                        }
                        ui.end_row();
                    });
                    ui.horizontal(|ui| {
                        if ui
                            .button("Sync-Status zurücksetzen")
                            .on_hover_text(
                                "Vergisst den letzten Sync. Der nächste 2-Wege-Sync verhält sich wie der erste: \
                                 nichts wird gelöscht, Unterschiede werden zu Konflikten.",
                            )
                            .clicked()
                        {
                            engine::reset_state(p);
                        }
                        if ui.button(RichText::new("Paar entfernen").color(RED)).clicked() {
                            remove = Some(i);
                        }
                    });
                });
                ui.add_space(4.0);
            }
            if let Some(i) = remove {
                self.cfg.pairs.remove(i);
            }
            if ui.button("➕ Ordnerpaar hinzufügen").clicked() {
                self.cfg.pairs.push(Pair::default());
            }

            ui.add_space(12.0);
            ui.heading("Allgemein");
            egui::Grid::new("general").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                ui.label("Auto-Sync-Intervall");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut self.cfg.interval_minutes).range(1..=1440));
                    ui.label("Minuten");
                });
                ui.end_row();
                ui.label("Massenlöschung ab");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut self.cfg.max_delete_percent).range(1..=100));
                    ui.label("% der Dateien: Löschungen nicht vorauswählen und warnen");
                });
                ui.end_row();
                ui.label("Inhaltsvergleich");
                ui.checkbox(&mut self.cfg.check_download, "herunterladen, wenn kein gemeinsamer Hash existiert (langsam)");
                ui.end_row();
                ui.label("rclone-Programm");
                ui.add(
                    egui::TextEdit::singleline(&mut self.cfg.rclone_path)
                        .desired_width(480.0)
                        .hint_text("leer = neben Draisine, sonst PATH"),
                );
                ui.end_row();
                ui.label("rclone.conf");
                ui.add(
                    egui::TextEdit::singleline(&mut self.cfg.rclone_config)
                        .desired_width(480.0)
                        .hint_text("leer = rclone.conf neben Draisine, sonst rclone-Standard"),
                );
                ui.end_row();
                ui.label("Zusätzliche rclone-Flags").on_hover_text("eins pro Zeile");
                let mut s = self.cfg.extra_flags.join("\n");
                if ui.add(egui::TextEdit::multiline(&mut s).desired_rows(2).desired_width(480.0)).changed() {
                    self.cfg.extra_flags = s.split('\n').map(str::to_string).collect();
                }
                ui.end_row();
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let dirty = self.cfg != self.saved;
                if ui.add_enabled(dirty, egui::Button::new(RichText::new("💾 Speichern").strong())).clicked() {
                    self.save();
                }
                if ui.add_enabled(dirty, egui::Button::new("Verwerfen")).clicked() {
                    self.cfg = self.saved.clone();
                }
                if dirty {
                    ui.label(RichText::new("ungespeicherte Änderungen").color(ORANGE));
                } else {
                    ui.label(RichText::new(&self.save_msg).weak());
                }
            });

            ui.add_space(12.0);
            ui.heading("rclone");
            ui.label(format!("Version: {}", self.rclone_version));
            ui.label(format!(
                "Remotes: {}",
                if self.remotes.is_empty() { "keine".to_string() } else { self.remotes.join(", ") }
            ));
            if ui.button("Neu laden").clicked() {
                self.load_rclone_info();
            }
            ui.add_space(4.0);
            ui.label("Neues Remote einrichten (der Browser öffnet sich zur Anmeldung):");
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut self.new_remote_name).desired_width(120.0));
                ui.label("Typ");
                egui::ComboBox::from_id_salt("rtype").selected_text(self.new_remote_type.clone()).show_ui(ui, |ui| {
                    for t in ["drive", "onedrive", "dropbox", "box", "pcloud", "jottacloud", "yandex"] {
                        ui.selectable_value(&mut self.new_remote_type, t.to_string(), t);
                    }
                });
                let can = !self.remote_setup_running && !self.new_remote_name.trim().is_empty();
                if ui.add_enabled(can, egui::Button::new("Einrichten")).clicked() {
                    self.remote_setup_running = true;
                    self.remote_setup_out = "Warte auf Anmeldung im Browser …".into();
                    let (tx, ctx, cfg) = (self.tx.clone(), self.ctx.clone(), self.saved.clone());
                    let (name, typ) = (self.new_remote_name.trim().to_string(), self.new_remote_type.clone());
                    std::thread::spawn(move || {
                        let out = match Rclone::new(&cfg) {
                            Ok(rc) => {
                                let mut args = vec!["config".to_string(), "create".into(), name.clone(), typ.clone()];
                                if typ == "drive" {
                                    args.push("scope=drive".into());
                                }
                                log::info(format!("Remote einrichten: {name} ({typ})"));
                                match rc.run(&args) {
                                    Ok((true, _, _)) => format!("✔ Remote „{name}:“ eingerichtet."),
                                    Ok((false, o, e)) => format!("✖ fehlgeschlagen:\n{o}\n{e}"),
                                    Err(e) => format!("✖ {e}"),
                                }
                            }
                            Err(e) => format!("✖ {e}"),
                        };
                        log::info(&out);
                        Self::send(&tx, &ctx, Msg::RemoteSetup(out));
                    });
                }
                if self.remote_setup_running {
                    ui.spinner();
                }
            });
            if !self.remote_setup_out.is_empty() {
                ui.monospace(&self.remote_setup_out);
            }
            ui.label(
                RichText::new(
                    "Tipp Google Drive: Ein eigener OAuth-Client (client_id/client_secret) ist deutlich schneller als \
                     rclone's gemeinsamer Standard-Client. Dafür einmal im Terminal „rclone config“ ausführen.",
                )
                .weak()
                .small(),
            );

            ui.add_space(12.0);
            ui.heading("Speicherorte");
            let d = dirs();
            ui.label(format!("Konfiguration: {}", d.config_file.display()));
            ui.label(format!("Sync-Status: {}", d.state_dir.display()));
            ui.label(format!("Logs: {}", d.log_dir.display()));
            ui.label(
                RichText::new(if d.portable {
                    "Portabler Modus: alle Daten liegen im Ordner draisine-data neben dem Programm.".to_string()
                } else {
                    format!(
                        "Tipp: Für den portablen Modus einen Ordner „{}“ neben dem Programm anlegen.",
                        crate::config::PORTABLE_DIR
                    )
                })
                .weak(),
            );
        });
    }

    // ------------------------------------------------------------ logs tab

    fn logs_tab(&mut self, ui: &mut egui::Ui) {
        if self.log_loaded.elapsed() > Duration::from_secs(2) {
            self.log_text = log::tail(1000);
            self.log_loaded = Instant::now();
        }
        ui.horizontal(|ui| {
            if ui.button("📄 Log als TXT öffnen").clicked() {
                log::open_today();
            }
            if ui.button("📁 Log-Ordner öffnen").clicked() {
                log::open_dir();
            }
            ui.label(RichText::new(log::today_file().display().to_string()).weak());
        });
        ui.separator();
        egui::ScrollArea::both().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(&self.log_text).monospace().small()).extend());
        });
    }
}

impl eframe::App for Gui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        if let Some(m) = self.pending_replan.take() {
            self.start_plan(m);
        }
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new("Draisine").strong());
                ui.add_space(16.0);
                ui.selectable_value(&mut self.tab, Tab::Sync, "Sync");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Einstellungen");
                ui.selectable_value(&mut self.tab, Tab::Logs, "Logs");
            });
        });
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Sync => self.sync_tab(ui),
            Tab::Settings => self.settings_tab(ui),
            Tab::Logs => self.logs_tab(ui),
        });
        self.confirm_modal(&ctx);
    }
}
