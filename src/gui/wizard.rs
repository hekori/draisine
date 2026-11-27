//! First-run setup assistant: rclone check ➡ connection (provider + browser login) ➡ folders ➡ done.

use super::Bg;
use super::browser::RemoteBrowser;
use super::theme::{self, pal};
use crate::config::{Config, Pair};
use crate::engine::Mode;
use crate::log;
use crate::rclone::Rclone;
use eframe::egui::{self, RichText, Ui, vec2};
use std::collections::HashMap;
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

const LABELS: [&str; 6] = ["Welcome", "rclone", "Provider", "Sign in", "Folders", "Done"];

struct Provider {
    icon: &'static str,
    name: &'static str,
    typ: &'static str,
    default_name: &'static str,
    /// Empty: OAuth sign-in in the browser. Otherwise: parameters entered in a form.
    fields: &'static [Field],
}

/// One `key=value` parameter for `rclone config create`.
struct Field {
    key: &'static str,
    label: &'static str,
    hint: &'static str,
    secret: bool,
    required: bool,
    /// (rclone value, label); non-empty = drop-down, the first entry is the default.
    choices: &'static [(&'static str, &'static str)],
}

const fn text(key: &'static str, label: &'static str, hint: &'static str, required: bool) -> Field {
    Field { key, label, hint, secret: false, required, choices: &[] }
}

const fn secret(key: &'static str, label: &'static str, hint: &'static str, required: bool) -> Field {
    Field { key, label, hint, secret: true, required, choices: &[] }
}

const fn choice(key: &'static str, label: &'static str, choices: &'static [(&'static str, &'static str)]) -> Field {
    Field { key, label, hint: "", secret: false, required: true, choices }
}

const S3_FIELDS: &[Field] = &[
    choice(
        "provider",
        "Provider",
        &[
            ("AWS", "Amazon S3"),
            ("Cloudflare", "Cloudflare R2"),
            ("Wasabi", "Wasabi"),
            ("Hetzner", "Hetzner Object Storage"),
            ("IDrive", "IDrive e2"),
            ("Scaleway", "Scaleway"),
            ("DigitalOcean", "DigitalOcean Spaces"),
            ("Minio", "MinIO"),
            ("Other", "Other S3-compatible"),
        ],
    ),
    text("access_key_id", "Access key ID", "", true),
    secret("secret_access_key", "Secret access key", "", true),
    text("region", "Region", "e.g. eu-central-1 (optional)", false),
    // Required for everything but AWS, see `missing_field`.
    text("endpoint", "Endpoint", "e.g. https://s3.example.com", false),
];

const WEBDAV_FIELDS: &[Field] = &[
    text("url", "Server URL", "e.g. https://cloud.example.com/remote.php/dav/files/USER", true),
    choice(
        "vendor",
        "Server type",
        &[("nextcloud", "Nextcloud"), ("owncloud", "ownCloud"), ("fastmail", "Fastmail Files"), ("other", "Other WebDAV")],
    ),
    text("user", "User name", "", true),
    secret("pass", "Password", "app password recommended", true),
];

const SFTP_FIELDS: &[Field] = &[
    text("host", "Server", "e.g. nas.example.com", true),
    text("port", "Port", "22", false),
    text("user", "User name", "", true),
    secret("pass", "Password", "leave empty to use an SSH key", false),
    text("key_file", "SSH key file", "e.g. ~/.ssh/id_ed25519 (empty: ssh-agent)", false),
];

const PROVIDERS: [Provider; 9] = [
    Provider { icon: "logo:drive", name: "Google Drive", typ: "drive", default_name: "gdrive", fields: &[] },
    Provider { icon: "logo:onedrive", name: "OneDrive", typ: "onedrive", default_name: "onedrive", fields: &[] },
    Provider { icon: "logo:dropbox", name: "Dropbox", typ: "dropbox", default_name: "dropbox", fields: &[] },
    Provider { icon: "☁", name: "pCloud", typ: "pcloud", default_name: "pcloud", fields: &[] },
    Provider { icon: "📦", name: "Box", typ: "box", default_name: "box", fields: &[] },
    Provider { icon: "logo:s3", name: "S3", typ: "s3", default_name: "s3", fields: S3_FIELDS },
    Provider { icon: "logo:webdav", name: "WebDAV", typ: "webdav", default_name: "webdav", fields: WEBDAV_FIELDS },
    Provider { icon: "logo:sftp", name: "SFTP", typ: "sftp", default_name: "sftp", fields: SFTP_FIELDS },
    Provider { icon: "⚙", name: "Other …", typ: "", default_name: "", fields: &[] },
];

impl Provider {
    fn is_form(&self) -> bool {
        !self.fields.is_empty()
    }
}

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
    /// Form values, keyed "<type>.<field key>" so switching provider keeps them.
    form: HashMap<String, String>,
    login_job: Option<Bg<Result<(), String>>>,
    child: Arc<Mutex<Option<Child>>>,
    login: Option<Result<(), String>>,
    /// Remote that is ready to use (after login or chosen existing).
    remote: String,
    local: String,
    browser: RemoteBrowser,
    pair_name: String,
    direction: Mode,
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
            form: HashMap::new(),
            login_job: None,
            child: Arc::new(Mutex::new(None)),
            login: None,
            remote: String::new(),
            local: String::new(),
            browser: RemoteBrowser::new("", ""),
            pair_name: String::new(),
            direction: Mode::TwoWay,
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

    /// Current value of a form field (choice fields default to their first entry).
    fn value(&self, typ: &str, f: &Field) -> String {
        match self.form.get(&format!("{typ}.{}", f.key)) {
            Some(v) => v.trim().to_string(),
            None => f.choices.first().map(|c| c.0.to_string()).unwrap_or_default(),
        }
    }

    /// Non-empty parameters for `rclone config create`.
    fn params(&self) -> Vec<(String, String)> {
        let pr = &PROVIDERS[self.provider];
        pr.fields
            .iter()
            .map(|f| (f.key.to_string(), self.value(pr.typ, f)))
            .filter(|(_, v)| !v.is_empty())
            .collect()
    }

    /// Label of the first required field that is still empty.
    fn missing_field(&self) -> Option<&'static str> {
        let pr = &PROVIDERS[self.provider];
        pr.fields.iter().find_map(|f| {
            let required = f.required || (pr.typ == "s3" && f.key == "endpoint" && self.value("s3", &S3_FIELDS[0]) != "AWS");
            (required && self.value(pr.typ, f).is_empty()).then_some(f.label)
        })
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
            let mut labels = LABELS;
            if self.existing.is_none() && PROVIDERS[self.provider].is_form() {
                labels[3] = "Details";
            }
            theme::steps(ui, &labels, self.step as usize);
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
                    !PROVIDERS[self.provider].typ.is_empty()
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
            if can_back && theme::secondary(ui, "Back").clicked() {
                self.step = match self.step {
                    Step::Rclone => Step::Welcome,
                    Step::Connection => Step::Rclone,
                    Step::Login => Step::Connection,
                    Step::Folders if self.existing.is_some() => Step::Connection,
                    Step::Folders => Step::Login,
                    s => s,
                };
            }
            if self.step != Step::Done && theme::link(ui, "Cancel").clicked() {
                self.cancel_login();
                out = Outcome::Cancel;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = match self.step {
                    Step::Welcome => "Let's go",
                    Step::Folders => "Create folder pair",
                    Step::Done => "Go to overview",
                    _ => "Next",
                };
                if self.step == Step::Done {
                    if theme::secondary(ui, label).clicked() {
                        out = Outcome::Finished { diff_pair: None };
                    }
                    if theme::primary(ui, "🔍  Show differences").clicked() {
                        out = Outcome::Finished { diff_pair: self.created_pair.clone() };
                    }
                    return;
                }
                if theme::primary_enabled(ui, self.can_next(), label).clicked() {
                    self.next(cfg);
                }
                if self.step == Step::Folders && self.first == Step::Connection {
                    // Started from "new connection": folders are optional.
                    if theme::link(ui, "Finish without a folder pair").clicked() {
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
                let pair = Pair { name, local, remote: self.browser.full(), direction: self.direction, ..Default::default() };
                let arrow = direction_arrow(pair.direction());
                log::info(format!("Folder pair created: {} ({} {arrow} {})", pair.label(), pair.local, pair.remote));
                self.created_pair = Some(pair.id.clone());
                cfg.pairs.push(pair);
                if let Err(e) = cfg.save() {
                    log::error(format!("Saving configuration: {e}"));
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
        ui.label(theme::bold("Welcome to Draisine", 26.0));
        ui.label(theme::muted(ui, "In a few steps you'll connect a folder on your computer with your cloud storage."));
        ui.add_space(14.0);
        for (icon, title, text) in [
            ("🔍", "Look first, then act", "Before every sync you see exactly what will happen. Nothing is deleted or overwritten without your approval."),
            ("🛡", "Nothing gets lost", "Deleted files go to the trash. If a file was changed on both sides, both versions are kept."),
            ("🔄", "Runs in the background", "If you like, Draisine syncs automatically and lets you know when your confirmation is needed."),
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
        ui.label(theme::bold("Checking the transfer tool", 22.0));
        ui.label(theme::muted(ui, "Draisine uses rclone to exchange files with your cloud storage."));
        ui.add_space(12.0);
        match &self.info {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Looking for rclone …");
                });
            }
            Some(Ok((v, remotes))) => {
                theme::callout(ui, p.green, |ui| {
                    ui.label(theme::bold("✔  rclone is ready", 15.0).color(p.green));
                    ui.label(theme::muted(ui, v));
                    if !remotes.is_empty() {
                        let names: Vec<String> = remotes.iter().map(|r| format!("{} ({})", r.0, r.1)).collect();
                        ui.label(theme::muted(ui, format!("Existing connections: {}", names.join(", "))));
                    }
                });
            }
            Some(Err(e)) => {
                theme::callout(ui, p.red, |ui| {
                    ui.label(theme::bold("rclone was not found", 15.0).color(p.red));
                    ui.label(e.as_str());
                });
                ui.add_space(8.0);
                ui.label("What to do:");
                ui.label("1.  Download rclone from the official site.");
                ui.label("2.  Put the file “rclone” (Windows: “rclone.exe”) in the same folder as Draisine.");
                ui.label("3.  Click “Check again”.");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if theme::secondary(ui, "🌐  Open rclone.org/downloads").clicked() {
                        let _ = open::that_detached("https://rclone.org/downloads/");
                    }
                    if theme::secondary(ui, "⟳  Check again").clicked() {
                        self.load_info(ui.ctx(), cfg);
                    }
                });
            }
        }
    }

    fn connection(&mut self, ui: &mut Ui) {
        ui.label(theme::bold("Which cloud storage do you want to connect?", 22.0));
        let remotes = self.remotes();
        if !remotes.is_empty() {
            ui.label(theme::muted(ui, "You can use an existing connection or create a new one."));
            theme::section(ui, "Existing connections");
            ui.horizontal_wrapped(|ui| {
                for (n, t) in &remotes {
                    let sel = self.existing.as_deref() == Some(n.as_str());
                    let icon = if theme::has_logo(t) { format!("logo:{t}") } else { "🔗".to_string() };
                    if theme::choice_card(ui, vec2(170.0, 96.0), sel, &icon, n, theme::provider_name(t)).clicked() {
                        self.existing = Some(n.clone());
                    }
                }
            });
            ui.add_space(6.0);
            theme::section(ui, "New connection");
        } else {
            ui.label(theme::muted(ui, "Choose your provider. Then you sign in or enter the connection details."));
            ui.add_space(6.0);
        }
        ui.horizontal_wrapped(|ui| {
            for (i, pr) in PROVIDERS.iter().enumerate() {
                let sel = self.existing.is_none() && self.provider == i;
                let sub = match pr.typ {
                    "s3" => "R2, Wasabi, MinIO …",
                    "webdav" => "Nextcloud, ownCloud …",
                    "sftp" => "Server, NAS",
                    _ => "",
                };
                if theme::choice_card(ui, vec2(170.0, 96.0), sel, pr.icon, pr.name, sub).clicked() {
                    self.existing = None;
                    self.provider = i;
                    if !pr.default_name.is_empty() {
                        self.name = unique_name(pr.default_name, &remotes);
                    }
                }
            }
        });
        if self.existing.is_none() && PROVIDERS[self.provider].typ.is_empty() {
            let p = pal(ui);
            ui.add_space(8.0);
            theme::callout(ui, p.blue, |ui| {
                ui.label(theme::bold("Other providers (FTP, Mega, Koofr, Proton Drive …)", 14.5));
                ui.label("rclone supports over 70 providers. Set yours up once in a terminal:");
                ui.monospace("rclone config");
                ui.label("Then click “Back” and “Next” again: the new connection appears above.");
                if theme::link(ui, "Open the guide on rclone.org").clicked() {
                    let _ = open::that_detached("https://rclone.org/docs/");
                }
            });
        }
    }

    /// Connection name input with validation hint.
    fn name_ui(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let remotes = self.remotes();
        ui.horizontal(|ui| {
            ui.label("Connection name");
            ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(200.0));
        });
        let n = self.name.trim();
        if !valid_name(n) {
            ui.label(RichText::new("Use only letters, digits, - and _.").color(p.orange).small());
        } else if remotes.iter().any(|r| r.0 == n) {
            ui.label(RichText::new("This name already exists.").color(p.orange).small());
        } else {
            ui.label(theme::muted(ui, format!("Folders will later be addressed as e.g. “{n}:Documents”.")).small());
        }
    }

    /// Name and form entries are complete and valid.
    fn entries_ok(&self) -> bool {
        let n = self.name.trim();
        valid_name(n) && !self.remotes().iter().any(|r| r.0 == n) && self.missing_field().is_none()
    }

    /// Input fields of a parameter-based provider (S3, WebDAV, SFTP).
    fn form_ui(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let pr = &PROVIDERS[self.provider];
        let s3_custom = pr.typ == "s3" && self.value("s3", &S3_FIELDS[0]) != "AWS";
        egui::Grid::new(("wiz-form", pr.typ)).num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            for f in pr.fields {
                let key = format!("{}.{}", pr.typ, f.key);
                let required = f.required || (pr.typ == "s3" && f.key == "endpoint" && s3_custom);
                ui.label(if required { format!("{} *", f.label) } else { f.label.to_string() });
                if f.choices.is_empty() {
                    let v = self.form.entry(key).or_default();
                    let mut hint = f.hint.to_string();
                    if pr.typ == "s3" && f.key == "endpoint" && !s3_custom {
                        hint = "leave empty for Amazon S3".into();
                    }
                    ui.add(egui::TextEdit::singleline(v).password(f.secret).hint_text(hint).desired_width(360.0));
                } else {
                    let cur = self.value(pr.typ, f);
                    let shown = f.choices.iter().find(|c| c.0 == cur).map_or(cur.as_str(), |c| c.1);
                    egui::ComboBox::from_id_salt(&key).selected_text(shown).width(360.0).show_ui(ui, |ui| {
                        for (val, label) in f.choices {
                            if ui.selectable_label(cur == *val, *label).clicked() {
                                self.form.insert(key.clone(), val.to_string());
                            }
                        }
                    });
                }
                ui.end_row();
            }
        });
        ui.add_space(4.0);
        match self.missing_field() {
            Some(l) => ui.label(RichText::new(format!("Please fill in “{l}”.")).color(p.orange).small()),
            None => ui.label(theme::muted(ui, "Everything is saved in rclone's configuration file (passwords obscured).").small()),
        };
    }

    fn login(&mut self, ui: &mut Ui, cfg: &Config) {
        let p = pal(ui);
        let pr = &PROVIDERS[self.provider];
        if pr.is_form() {
            ui.label(theme::bold(format!("Connect to {}", pr.name), 22.0));
            ui.label(theme::muted(ui, "Enter the connection details (* = required). Draisine saves the connection and checks that it works."));
        } else {
            ui.label(theme::bold(format!("Sign in to {}", pr.name), 22.0));
            ui.label(theme::muted(ui, "Your browser will open. Sign in there and allow access. Then this continues automatically."));
        }
        ui.add_space(12.0);
        let editable = self.login_job.is_none() && !matches!(self.login, Some(Ok(())));
        ui.add_enabled_ui(editable, |ui| {
            self.name_ui(ui);
            if pr.is_form() {
                ui.add_space(8.0);
                self.form_ui(ui);
            }
        });
        ui.add_space(12.0);
        match (&self.login, &self.login_job) {
            (_, Some(_)) => {
                theme::callout(ui, p.blue, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        let wait = if pr.is_form() { "Connecting …" } else { "Waiting for sign-in in the browser …" };
                        ui.label(theme::bold(wait, 15.0));
                    });
                    if !pr.is_form() {
                        ui.label(theme::muted(ui, "If no browser opened: the log contains a link you can copy."));
                    }
                });
                ui.add_space(6.0);
                if theme::secondary(ui, "Cancel").clicked() {
                    self.cancel_login();
                }
            }
            (Some(Ok(())), None) => {
                theme::callout(ui, p.green, |ui| {
                    ui.label(theme::bold(format!("✔  Connected: {}", self.remote), 15.0).color(p.green));
                    ui.label(theme::muted(ui, "The connection works. Next: choose folders."));
                });
            }
            (res, None) => {
                if let Some(Err(e)) = res {
                    theme::callout(ui, p.red, |ui| {
                        ui.label(theme::bold(if pr.is_form() { "Connection failed" } else { "Sign-in failed" }, 15.0).color(p.red));
                        ui.label(e.as_str());
                    });
                    ui.add_space(8.0);
                }
                if pr.typ == "drive" {
                    ui.label(theme::muted(
                        ui,
                        "Note: Google may show “Google hasn't verified this app”. That is normal for rclone; \
                         continue via “Advanced” ➡ “Go to rclone”.",
                    ));
                    ui.add_space(6.0);
                }
                let label = match (res.is_some(), pr.is_form()) {
                    (true, _) => "🔑  Try again",
                    (false, true) => "🔌  Connect",
                    (false, false) => "🔑  Sign in in browser",
                };
                if theme::primary_enabled(ui, self.entries_ok(), label).clicked() {
                    self.start_login(ui.ctx(), cfg);
                }
            }
        }
    }

    fn start_login(&mut self, ctx: &egui::Context, cfg: &Config) {
        let (cfg, name, typ) = (cfg.clone(), self.name.trim().to_string(), PROVIDERS[self.provider].typ.to_string());
        let params = PROVIDERS[self.provider].is_form().then(|| self.params());
        let child = self.child.clone();
        self.login = None;
        self.login_job = Some(Bg::spawn(ctx, move || {
            let rc = Rclone::new(&cfg).map_err(|e| e.to_string())?;
            let c = match &params {
                Some(ps) => rc.spawn_config_create_params(&name, &typ, ps),
                None => rc.spawn_config_create(&name, &typ),
            }
            .map_err(|e| e.to_string())?;
            *child.lock().unwrap() = Some(c);
            let status = loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                let mut g = child.lock().unwrap();
                match g.as_mut().map(|c| c.try_wait()) {
                    Some(Ok(Some(s))) => break s,
                    Some(Ok(None)) => continue,
                    _ => return Err("Cancelled".into()),
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
                let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("cancelled").to_string();
                return Err(last);
            }
            match rc.test_remote(&name) {
                Ok(()) => Ok(()),
                // Typed-in parameters are probably wrong: remove the connection so the
                // user can go back, correct them and create it again under the same name.
                Err(e) if params.is_some() => {
                    let _ = rc.run(&["config".into(), "delete".into(), name.clone()]);
                    Err(format!("Could not connect: {e}"))
                }
                Err(e) => Err(format!("Connection created, but the test failed: {e}")),
            }
        }));
    }

    fn folders(&mut self, ui: &mut Ui, cfg: &Config) {
        let p = pal(ui);
        ui.label(theme::bold("Which folders should stay in sync?", 22.0));
        ui.label(theme::muted(ui, "Choose a folder on your computer and the matching folder in the cloud."));
        ui.add_space(10.0);

        ui.label(theme::bold("💻  On this computer", 15.0));
        ui.horizontal(|ui| {
            let txt = if self.local.is_empty() { "no folder chosen yet".to_string() } else { self.local.clone() };
            egui::Frame::new()
                .fill(p.surface2)
                .stroke(egui::Stroke::new(1.0, p.border))
                .corner_radius(8)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_width((ui.available_width() - 170.0).max(100.0));
                    ui.label(if self.local.is_empty() { theme::muted(ui, txt) } else { RichText::new(txt) });
                });
            if theme::secondary(ui, "📂  Choose folder …").clicked()
                && let Some(d) = rfd::FileDialog::new().set_title("Choose local folder").pick_folder() {
                    self.local = d.display().to_string();
                    if self.pair_name.is_empty() {
                        self.pair_name = default_pair_name(&self.local);
                    }
                }
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.label(theme::bold("☁  In the cloud", 15.0));
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
        ui.label(theme::muted(ui, "Click through to the folder you want. The folder shown is the one used."));
        self.browser.ui(ui, cfg, 200.0);
        let shown = if self.browser.path.is_empty() { format!("{} (top level)", self.browser.full()) } else { self.browser.full() };
        ui.label(RichText::new(format!("Selected: {shown}")).color(p.accent));
        ui.add_space(10.0);
        ui.label(theme::bold("Direction", 15.0));
        super::direction_picker(ui, &mut self.direction);
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label("Folder pair name");
            ui.add(egui::TextEdit::singleline(&mut self.pair_name).desired_width(240.0).hint_text("e.g. Taxes"));
        });
    }

    fn done(&mut self, ui: &mut Ui, cfg: &mut Config) {
        let p = pal(ui);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("✔").size(44.0).color(p.green));
            ui.label(theme::bold("All set!", 26.0));
            if let Some(pair) = cfg.pairs.last() {
                ui.label(theme::muted(ui, format!("{}  {}  {}", pair.local, direction_arrow(pair.direction()), pair.remote)));
            }
        });
        ui.add_space(14.0);
        ui.label(theme::bold("What's next?", 15.0));
        ui.label("Recommendation: look at the differences first. Nothing is changed while you do.");
        ui.add_space(8.0);
        theme::card(ui, |ui| {
            ui.label(theme::bold("🔄  Sync automatically", 14.5));
            ui.label(theme::muted(
                ui,
                "After the first sync, which you do by hand, you can turn on auto-sync on the folder \
                 pair's card and choose how often it runs. Deletes and overwrites are applied when only one \
                 side changed since the last sync; anything unclear waits for you.",
            ));
        });
    }
}

/// Arrow between “local” and “remote” for a direction.
fn direction_arrow(d: Mode) -> &'static str {
    match d {
        Mode::Pull => "⬅",
        Mode::Push => "➡",
        _ => "↔",
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
