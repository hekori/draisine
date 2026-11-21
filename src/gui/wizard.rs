//! First-run setup assistant: rclone check ➡ connection (provider + browser login) ➡ folders ➡ done.

use super::Bg;
use super::browser::RemoteBrowser;
use super::theme::{self, pal};
use crate::config::{Config, Pair};
use crate::log;
use crate::rclone::Rclone;
use eframe::egui::{self, RichText, Ui, vec2};
use std::process::Child;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
pub enum Step {
    Welcome = 0,
    Rclone = 1,
    Connection = 2,
    Login = 3,
    Folders = 4,
    Done = 5,
}

const LABELS: [&str; 6] = ["Willkommen", "rclone", "Anbieter", "Anmelden", "Ordner", "Fertig"];

struct Provider {
    icon: &'static str,
    name: &'static str,
    typ: &'static str,
    default_name: &'static str,
}

const PROVIDERS: [Provider; 6] = [
    Provider { icon: "🔺", name: "Google Drive", typ: "drive", default_name: "gdrive" },
    Provider { icon: "☁", name: "OneDrive", typ: "onedrive", default_name: "onedrive" },
    Provider { icon: "📦", name: "Dropbox", typ: "dropbox", default_name: "dropbox" },
    Provider { icon: "☁", name: "pCloud", typ: "pcloud", default_name: "pcloud" },
    Provider { icon: "📦", name: "Box", typ: "box", default_name: "box" },
    Provider { icon: "⚙", name: "Andere …", typ: "", default_name: "" },
];

pub enum Outcome {
    Continue,
    Cancel,
    /// Setup finished; optionally start a diff of the new pair.
    Finished { diff_pair: Option<String> },
}

type Info = Result<(String, Vec<(String, String)>), String>;

pub struct Wizard {
    step: Step,
    first: Step,
    info: Option<Info>,
    info_job: Option<Bg<Info>>,
    /// Chosen existing remote (name), or None = create new.
    existing: Option<String>,
    provider: usize,
    name: String,
    login_job: Option<Bg<Result<(), String>>>,
    child: Arc<Mutex<Option<Child>>>,
    login: Option<Result<(), String>>,
    /// Remote that is ready to use (after login or chosen existing).
    remote: String,
    local: String,
    browser: RemoteBrowser,
    pair_name: String,
    auto: bool,
    created_pair: Option<String>,
    /// Started via "new connection": do not preselect an existing one.
    prefer_new: bool,
}

impl Wizard {
    pub fn new(start: Step, prefer_new: bool) -> Wizard {
        Wizard {
            step: start,
            first: start,
            info: None,
            info_job: None,
            existing: None,
            provider: 0,
            name: "gdrive".into(),
            login_job: None,
            child: Arc::new(Mutex::new(None)),
            login: None,
            remote: String::new(),
            local: String::new(),
            browser: RemoteBrowser::new("", ""),
            pair_name: String::new(),
            auto: false,
            created_pair: None,
            prefer_new,
        }
    }

    /// Debug builds only: jump to a step for visual checks.
    #[cfg(debug_assertions)]
    pub fn debug_at(n: usize) -> Wizard {
        let steps = [Step::Welcome, Step::Rclone, Step::Connection, Step::Login, Step::Folders, Step::Done];
        let mut w = Wizard::new(Step::Welcome, false);
        w.step = steps[n.min(5)];
        if w.step >= Step::Folders {
            w.remote = "gdrive".into();
            w.browser = RemoteBrowser::new("gdrive", "");
        }
        w
    }

    fn remotes(&self) -> Vec<(String, String)> {
        match &self.info {
            Some(Ok((_, r))) => r.clone(),
            _ => Vec::new(),
        }
    }

    fn load_info(&mut self, ctx: &egui::Context, cfg: &Config) {
        let cfg = cfg.clone();
        self.info = None;
        self.info_job = Some(Bg::spawn(ctx, move || {
            let rc = Rclone::new(&cfg).map_err(|e| e.to_string())?;
            Ok((rc.version(), rc.remotes()))
        }));
    }

    /// Polls background jobs; call once per frame before `body` / `footer`.
    pub fn poll(&mut self, ctx: &egui::Context, cfg: &Config) {
        if self.info.is_none() && self.info_job.is_none() {
            self.load_info(ctx, cfg);
        }
        if let Some(j) = &mut self.info_job
            && let Some(r) = j.poll() {
                self.info_job = None;
                if self.existing.is_none() && !self.prefer_new && self.login.is_none()
                    && let Ok((_, rs)) = &r {
                        self.existing = rs.first().map(|x| x.0.clone());
                    }
                self.info = Some(r);
            }
        if let Some(j) = &mut self.login_job
            && let Some(r) = j.poll() {
                self.login_job = None;
                if r.is_ok() {
                    self.remote = self.name.trim().to_string();
                }
                self.login = Some(r);
                self.load_info(ctx, cfg);
            }
    }

    /// Runs `add` in a centred column of at most 760 px.
    fn centred<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
        let width = ui.available_width().min(760.0);
        let margin = ((ui.available_width() - width) / 2.0).max(0.0);
        ui.horizontal(|ui| {
            ui.add_space(margin);
            ui.vertical(|ui| {
                ui.set_width(width);
                ui.set_max_width(width);
                add(ui)
            })
            .inner
        })
        .inner
    }

    /// Step indicator + content card (scrollable area).
    pub fn body(&mut self, ui: &mut Ui, cfg: &mut Config) {
        Self::centred(ui, |ui| {
            ui.add_space(12.0);
            theme::steps(ui, &LABELS, self.step as usize);
            ui.add_space(18.0);
            theme::card(ui, |ui| match self.step {
                Step::Welcome => self.welcome(ui),
                Step::Rclone => self.rclone(ui, cfg),
                Step::Connection => self.connection(ui),
                Step::Login => self.login(ui, cfg),
                Step::Folders => self.folders(ui, cfg),
                Step::Done => self.done(ui, cfg),
            });
            ui.add_space(12.0);
        });
    }

    /// Back / next buttons (fixed footer).
    pub fn footer(&mut self, ui: &mut Ui, cfg: &mut Config) -> Outcome {
        Self::centred(ui, |ui| {
            ui.add_space(12.0);
            let out = self.nav(ui, cfg);
            ui.add_space(12.0);
            out
        })
    }

    // ---------------------------------------------------------------- navigation

    fn can_next(&self) -> bool {
        match self.step {
            Step::Welcome => true,
            Step::Rclone => matches!(self.info, Some(Ok(_))),
            Step::Connection => {
                if self.existing.is_some() {
                    true
                } else {
                    let n = self.name.trim();
                    !PROVIDERS[self.provider].typ.is_empty()
                        && valid_name(n)
                        && !self.remotes().iter().any(|r| r.0 == n)
                }
            }
            Step::Login => matches!(self.login, Some(Ok(()))),
            Step::Folders => !self.local.trim().is_empty() && !self.remote.is_empty(),
            Step::Done => true,
        }
    }

    fn nav(&mut self, ui: &mut Ui, cfg: &mut Config) -> Outcome {
        let mut out = Outcome::Continue;
        ui.horizontal(|ui| {
            let can_back = self.step > self.first && self.step != Step::Done && self.login_job.is_none();
            if can_back && theme::secondary(ui, "Zurück").clicked() {
                self.step = match self.step {
                    Step::Rclone => Step::Welcome,
                    Step::Connection => Step::Rclone,
                    Step::Login => Step::Connection,
                    Step::Folders if self.existing.is_some() => Step::Connection,
                    Step::Folders => Step::Login,
                    s => s,
                };
            }
            if self.step != Step::Done && theme::link(ui, "Abbrechen").clicked() {
                self.cancel_login();
                out = Outcome::Cancel;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = match self.step {
                    Step::Welcome => "Los geht's",
                    Step::Folders => "Ordnerpaar anlegen",
                    Step::Done => "Zur Übersicht",
                    _ => "Weiter",
                };
                if self.step == Step::Done {
                    if theme::secondary(ui, label).clicked() {
                        out = Outcome::Finished { diff_pair: None };
                    }
                    if theme::primary(ui, "🔍  Unterschiede anzeigen").clicked() {
                        out = Outcome::Finished { diff_pair: self.created_pair.clone() };
                    }
                    return;
                }
                if theme::primary_enabled(ui, self.can_next(), label).clicked() {
                    self.next(cfg);
                }
                if self.step == Step::Folders && self.first == Step::Connection {
                    // Started from "new connection": folders are optional.
                    if theme::link(ui, "Ohne Ordnerpaar beenden").clicked() {
                        out = Outcome::Finished { diff_pair: None };
                    }
                }
            });
        });
        out
    }

    fn next(&mut self, cfg: &mut Config) {
        self.step = match self.step {
            Step::Welcome => Step::Rclone,
            Step::Rclone => Step::Connection,
            Step::Connection => {
                if let Some(r) = &self.existing {
                    self.remote = r.clone();
                    self.browser = RemoteBrowser::new(r, "");
                    Step::Folders
                } else {
                    self.login = None;
                    Step::Login
                }
            }
            Step::Login => {
                self.browser = RemoteBrowser::new(&self.remote, "");
                Step::Folders
            }
            Step::Folders => {
                let local = self.local.trim().to_string();
                let name = if self.pair_name.trim().is_empty() { default_pair_name(&local) } else { self.pair_name.trim().into() };
                let pair = Pair { name, local, remote: self.browser.full(), ..Default::default() };
                log::info(format!("Ordnerpaar angelegt: {} ({} ↔ {})", pair.label(), pair.local, pair.remote));
                self.created_pair = Some(pair.id.clone());
                self.auto = cfg.auto_sync;
                cfg.pairs.push(pair);
                if let Err(e) = cfg.save() {
                    log::error(format!("Konfiguration speichern: {e}"));
                }
                Step::Done
            }
            Step::Done => Step::Done,
        };
    }

    fn cancel_login(&mut self) {
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
    }

    // ---------------------------------------------------------------- steps

    fn welcome(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        ui.label(theme::bold("Willkommen bei Draisine", 26.0));
        ui.label(theme::muted(ui, "In ein paar Schritten verbindest du einen Ordner auf deinem Rechner mit deinem Cloud-Speicher."));
        ui.add_space(14.0);
        for (icon, title, text) in [
            ("🔍", "Erst ansehen, dann handeln", "Vor jedem Sync siehst du genau, was passieren wird. Nichts wird gelöscht oder überschrieben, ohne dass du zustimmst."),
            ("🛡", "Nichts geht verloren", "Gelöschte Dateien landen im Papierkorb. Wurde eine Datei auf beiden Seiten geändert, bleiben beide Versionen erhalten."),
            ("🔄", "Läuft im Hintergrund", "Auf Wunsch synchronisiert Draisine automatisch und meldet sich, wenn deine Bestätigung gebraucht wird."),
        ] {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon).size(22.0).color(p.accent));
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.label(theme::bold(title, 15.0));
                    ui.label(theme::muted(ui, text));
                });
            });
            ui.add_space(8.0);
        }
    }

    fn rclone(&mut self, ui: &mut Ui, cfg: &Config) {
        let p = pal(ui);
        ui.label(theme::bold("Transportwerkzeug prüfen", 22.0));
        ui.label(theme::muted(ui, "Draisine nutzt rclone, um Dateien mit dem Cloud-Speicher auszutauschen."));
        ui.add_space(12.0);
        match &self.info {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Suche rclone …");
                });
            }
            Some(Ok((v, remotes))) => {
                theme::callout(ui, p.green, |ui| {
                    ui.label(theme::bold("✔  rclone ist bereit", 15.0).color(p.green));
                    ui.label(theme::muted(ui, v));
                    if !remotes.is_empty() {
                        let names: Vec<String> = remotes.iter().map(|r| format!("{} ({})", r.0, r.1)).collect();
                        ui.label(theme::muted(ui, format!("Vorhandene Verbindungen: {}", names.join(", "))));
                    }
                });
            }
            Some(Err(e)) => {
                theme::callout(ui, p.red, |ui| {
                    ui.label(theme::bold("rclone wurde nicht gefunden", 15.0).color(p.red));
                    ui.label(e.as_str());
                });
                ui.add_space(8.0);
                ui.label("So geht's weiter:");
                ui.label("1.  rclone von der offiziellen Seite herunterladen.");
                ui.label("2.  Die Datei „rclone“ (Windows: „rclone.exe“) in denselben Ordner wie Draisine legen.");
                ui.label("3.  Auf „Erneut prüfen“ klicken.");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if theme::secondary(ui, "🌐  rclone.org/downloads öffnen").clicked() {
                        let _ = open::that_detached("https://rclone.org/downloads/");
                    }
                    if theme::secondary(ui, "⟳  Erneut prüfen").clicked() {
                        self.load_info(ui.ctx(), cfg);
                    }
                });
            }
        }
    }

    fn connection(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        ui.label(theme::bold("Mit welchem Cloud-Speicher verbinden?", 22.0));
        let remotes = self.remotes();
        if !remotes.is_empty() {
            ui.label(theme::muted(ui, "Du kannst eine vorhandene Verbindung verwenden oder eine neue anlegen."));
            theme::section(ui, "Vorhandene Verbindungen");
            ui.horizontal_wrapped(|ui| {
                for (n, t) in &remotes {
                    let sel = self.existing.as_deref() == Some(n.as_str());
                    if theme::choice_card(ui, vec2(170.0, 96.0), sel, "🔗", n, t).clicked() {
                        self.existing = Some(n.clone());
                    }
                }
            });
            ui.add_space(6.0);
            theme::section(ui, "Neue Verbindung");
        } else {
            ui.label(theme::muted(ui, "Wähle deinen Anbieter. Im nächsten Schritt meldest du dich im Browser an."));
            ui.add_space(6.0);
        }
        ui.horizontal_wrapped(|ui| {
            for (i, pr) in PROVIDERS.iter().enumerate() {
                let sel = self.existing.is_none() && self.provider == i;
                if theme::choice_card(ui, vec2(170.0, 96.0), sel, pr.icon, pr.name, "").clicked() {
                    self.existing = None;
                    self.provider = i;
                    if !pr.default_name.is_empty() {
                        self.name = unique_name(pr.default_name, &remotes);
                    }
                }
            }
        });
        if self.existing.is_none() {
            ui.add_space(8.0);
            if PROVIDERS[self.provider].typ.is_empty() {
                theme::callout(ui, p.blue, |ui| {
                    ui.label(theme::bold("Andere Anbieter (WebDAV, S3, Nextcloud, SFTP …)", 14.5));
                    ui.label("rclone unterstützt über 70 Anbieter. Richte sie einmalig im Terminal ein:");
                    ui.monospace("rclone config");
                    ui.label("Danach einfach „Zurück“ und wieder „Weiter“: die neue Verbindung erscheint oben.");
                    if theme::link(ui, "Anleitung auf rclone.org öffnen").clicked() {
                        let _ = open::that_detached("https://rclone.org/docs/");
                    }
                });
            } else {
                ui.horizontal(|ui| {
                    ui.label("Name der Verbindung");
                    ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(200.0));
                });
                let n = self.name.trim();
                if !valid_name(n) {
                    ui.label(RichText::new("Nur Buchstaben, Ziffern, - und _ verwenden.").color(p.orange).small());
                } else if remotes.iter().any(|r| r.0 == n) {
                    ui.label(RichText::new("Diesen Namen gibt es schon.").color(p.orange).small());
                } else {
                    ui.label(theme::muted(ui, format!("Ordner werden später z. B. als „{n}:Dokumente“ angesprochen.")).small());
                }
            }
        }
    }

    fn login(&mut self, ui: &mut Ui, cfg: &Config) {
        let p = pal(ui);
        let pr = &PROVIDERS[self.provider];
        ui.label(theme::bold(format!("Bei {} anmelden", pr.name), 22.0));
        ui.label(theme::muted(ui, "Dein Browser öffnet sich. Melde dich dort an und erlaube den Zugriff. Danach geht es hier automatisch weiter."));
        ui.add_space(12.0);
        match (&self.login, &self.login_job) {
            (_, Some(_)) => {
                theme::callout(ui, p.blue, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(theme::bold("Warte auf die Anmeldung im Browser …", 15.0));
                    });
                    ui.label(theme::muted(ui, "Falls sich kein Browser geöffnet hat: im Log steht ein Link zum Kopieren."));
                });
                ui.add_space(6.0);
                if theme::secondary(ui, "Abbrechen").clicked() {
                    self.cancel_login();
                }
            }
            (Some(Ok(())), None) => {
                theme::callout(ui, p.green, |ui| {
                    ui.label(theme::bold(format!("✔  Verbunden: {}", self.remote), 15.0).color(p.green));
                    ui.label(theme::muted(ui, "Die Verbindung funktioniert. Weiter zur Ordnerauswahl."));
                });
            }
            (res, None) => {
                if let Some(Err(e)) = res {
                    theme::callout(ui, p.red, |ui| {
                        ui.label(theme::bold("Anmeldung nicht erfolgreich", 15.0).color(p.red));
                        ui.label(e.as_str());
                    });
                    ui.add_space(8.0);
                }
                if pr.typ == "drive" {
                    ui.label(theme::muted(
                        ui,
                        "Hinweis: Google zeigt eventuell „Google hat diese App nicht überprüft“. Das ist bei rclone normal; \
                         über „Erweitert“ ➡ „Weiter zu rclone“ geht es weiter.",
                    ));
                    ui.add_space(6.0);
                }
                let label = if res.is_some() { "🔑  Erneut versuchen" } else { "🔑  Im Browser anmelden" };
                if theme::primary(ui, label).clicked() {
                    self.start_login(ui.ctx(), cfg);
                }
            }
        }
    }

    fn start_login(&mut self, ctx: &egui::Context, cfg: &Config) {
        let (cfg, name, typ) = (cfg.clone(), self.name.trim().to_string(), PROVIDERS[self.provider].typ.to_string());
        let child = self.child.clone();
        self.login = None;
        self.login_job = Some(Bg::spawn(ctx, move || {
            let rc = Rclone::new(&cfg).map_err(|e| e.to_string())?;
            let c = rc.spawn_config_create(&name, &typ).map_err(|e| e.to_string())?;
            *child.lock().unwrap() = Some(c);
            let status = loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                let mut g = child.lock().unwrap();
                match g.as_mut().map(|c| c.try_wait()) {
                    Some(Ok(Some(s))) => break s,
                    Some(Ok(None)) => continue,
                    _ => return Err("Abgebrochen".into()),
                }
            };
            let c = child.lock().unwrap().take();
            let mut err = String::new();
            if let Some(mut c) = c {
                use std::io::Read;
                if let Some(mut e) = c.stderr.take() {
                    let _ = e.read_to_string(&mut err);
                }
            }
            for l in err.lines() {
                log::info(format!("rclone: {l}"));
            }
            if !status.success() {
                let rcs = rc.remotes();
                if rcs.iter().any(|r| r.0 == name) {
                    // half-created remote: remove it so the user can retry cleanly
                    let _ = rc.run(&["config".into(), "delete".into(), name.clone()]);
                }
                let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("abgebrochen").to_string();
                return Err(last);
            }
            rc.test_remote(&name).map_err(|e| format!("Verbindung angelegt, aber Test fehlgeschlagen: {e}"))
        }));
    }

    fn folders(&mut self, ui: &mut Ui, cfg: &Config) {
        let p = pal(ui);
        ui.label(theme::bold("Welche Ordner sollen synchron bleiben?", 22.0));
        ui.label(theme::muted(ui, "Wähle einen Ordner auf deinem Rechner und den passenden Ordner in der Cloud."));
        ui.add_space(10.0);

        ui.label(theme::bold("💻  Auf diesem Rechner", 15.0));
        ui.horizontal(|ui| {
            let txt = if self.local.is_empty() { "noch kein Ordner gewählt".to_string() } else { self.local.clone() };
            egui::Frame::new()
                .fill(p.surface2)
                .stroke(egui::Stroke::new(1.0, p.border))
                .corner_radius(8)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_width((ui.available_width() - 170.0).max(100.0));
                    ui.label(if self.local.is_empty() { theme::muted(ui, txt) } else { RichText::new(txt) });
                });
            if theme::secondary(ui, "📂  Ordner wählen …").clicked()
                && let Some(d) = rfd::FileDialog::new().set_title("Lokalen Ordner wählen").pick_folder() {
                    self.local = d.display().to_string();
                    if self.pair_name.is_empty() {
                        self.pair_name = default_pair_name(&self.local);
                    }
                }
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.label(theme::bold("☁  In der Cloud", 15.0));
            let remotes = self.remotes();
            if remotes.len() > 1 {
                egui::ComboBox::from_id_salt("wiz-remote").selected_text(&self.remote).show_ui(ui, |ui| {
                    for (n, _) in &remotes {
                        if ui.selectable_label(self.remote == *n, n).clicked() {
                            self.remote = n.clone();
                            self.browser.set_remote(n);
                        }
                    }
                });
            }
        });
        ui.label(theme::muted(ui, "Klicke dich zum gewünschten Ordner. Der angezeigte Ordner wird verwendet."));
        self.browser.ui(ui, cfg, 200.0);
        let shown = if self.browser.path.is_empty() { format!("{} (oberste Ebene)", self.browser.full()) } else { self.browser.full() };
        ui.label(RichText::new(format!("Gewählt: {shown}")).color(p.accent));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label("Name des Ordnerpaars");
            ui.add(egui::TextEdit::singleline(&mut self.pair_name).desired_width(240.0).hint_text("z. B. Steuer"));
        });
    }

    fn done(&mut self, ui: &mut Ui, cfg: &mut Config) {
        let p = pal(ui);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("✔").size(44.0).color(p.green));
            ui.label(theme::bold("Alles bereit!", 26.0));
            if let Some(pair) = cfg.pairs.last() {
                ui.label(theme::muted(ui, format!("{}  ↔  {}", pair.local, pair.remote)));
            }
        });
        ui.add_space(14.0);
        ui.label(theme::bold("Wie geht es weiter?", 15.0));
        ui.label("Empfehlung: Sieh dir zuerst die Unterschiede an. Dabei wird nichts verändert.");
        ui.add_space(8.0);
        theme::card(ui, |ui| {
            ui.horizontal(|ui| {
                if theme::toggle(ui, &mut self.auto).changed() {
                    cfg.auto_sync = self.auto;
                    let _ = cfg.save();
                }
                ui.vertical(|ui| {
                    ui.label(theme::bold("Automatisch synchronisieren", 14.5));
                    ui.label(theme::muted(
                        ui,
                        format!(
                            "Alle {} Minuten im Hintergrund. Löschen und Überschreiben wartet immer auf dich.",
                            cfg.interval_minutes
                        ),
                    ));
                });
            });
        });
    }
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn unique_name(base: &str, existing: &[(String, String)]) -> String {
    if !existing.iter().any(|r| r.0 == base) {
        return base.into();
    }
    (2..).map(|i| format!("{base}{i}")).find(|n| !existing.iter().any(|r| &r.0 == n)).unwrap()
}

fn default_pair_name(local: &str) -> String {
    std::path::Path::new(local).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}
