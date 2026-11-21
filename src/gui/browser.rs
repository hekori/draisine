//! Folder browser for a remote (e.g. Google Drive), loads one level at a time in the background.

use super::Bg;
use super::theme::{self, pal};
use crate::config::Config;
use crate::rclone::Rclone;
use eframe::egui::{self, CursorIcon, RichText, Sense, Ui, vec2};

pub struct RemoteBrowser {
    pub remote: String,
    /// Path inside the remote, "" = root, "A/B" = sub folder.
    pub path: String,
    dirs: Vec<String>,
    error: String,
    loading: Option<Bg<Result<Vec<String>, String>>>,
    loaded_for: Option<(String, String)>,
}

impl RemoteBrowser {
    pub fn new(remote: &str, path: &str) -> Self {
        RemoteBrowser {
            remote: remote.trim_end_matches(':').to_string(),
            path: path.trim_matches('/').to_string(),
            dirs: Vec::new(),
            error: String::new(),
            loading: None,
            loaded_for: None,
        }
    }

    /// Full rclone path, e.g. `gdrive:STEUER/2025`.
    pub fn full(&self) -> String {
        format!("{}:{}", self.remote, self.path)
    }

    pub fn set_remote(&mut self, remote: &str) {
        let r = remote.trim_end_matches(':').to_string();
        if r != self.remote {
            self.remote = r;
            self.path.clear();
        }
    }

    fn reload(&mut self, ctx: &egui::Context, cfg: &Config) {
        let (cfg, full) = (cfg.clone(), self.full());
        self.loaded_for = Some((self.remote.clone(), self.path.clone()));
        self.error.clear();
        self.loading = Some(Bg::spawn(ctx, move || {
            let rc = Rclone::new(&cfg).map_err(|e| e.to_string())?;
            rc.list_dirs(&full).map_err(|e| e.to_string())
        }));
    }

    /// Renders the browser. The selected folder is `self.full()`.
    pub fn ui(&mut self, ui: &mut Ui, cfg: &Config, height: f32) {
        let p = pal(ui);
        if self.remote.is_empty() {
            ui.label(theme::muted(ui, "Bitte zuerst eine Verbindung wählen."));
            return;
        }
        if self.loaded_for.as_ref() != Some(&(self.remote.clone(), self.path.clone())) {
            self.reload(ui.ctx(), cfg);
        }
        if let Some(b) = &mut self.loading
            && let Some(r) = b.poll() {
                self.loading = None;
                match r {
                    Ok(d) => self.dirs = d,
                    Err(e) => {
                        self.dirs.clear();
                        self.error = e;
                    }
                }
            }

        // Breadcrumb
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if theme::link(ui, &format!("☁ {}", self.remote)).clicked() {
                self.path.clear();
            }
            let parts: Vec<String> = self.path.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
            for (i, part) in parts.iter().enumerate() {
                ui.label(theme::muted(ui, " / "));
                if i + 1 == parts.len() {
                    ui.label(theme::bold(part, 14.5));
                } else if theme::link(ui, part).clicked() {
                    self.path = parts[..=i].join("/");
                }
            }
        });

        egui::Frame::new()
            .fill(p.surface2)
            .stroke(egui::Stroke::new(1.0, p.border))
            .corner_radius(10)
            .inner_margin(6)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical().min_scrolled_height(height).max_height(height).auto_shrink([false, false]).show(ui, |ui| {
                    if !self.path.is_empty() && row(ui, "⬅", "eine Ebene höher", false).clicked() {
                        self.path = self.path.rsplit_once('/').map(|x| x.0.to_string()).unwrap_or_default();
                    }
                    if self.loading.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(theme::muted(ui, "Lade Ordner …"));
                        });
                    } else if !self.error.is_empty() {
                        ui.label(RichText::new(format!("✖ {}", self.error)).color(p.red));
                    } else if self.dirs.is_empty() {
                        ui.label(theme::muted(ui, "Keine Unterordner."));
                    }
                    let mut go = None;
                    for d in &self.dirs {
                        if row(ui, "", d, true).clicked() {
                            go = Some(d.clone());
                        }
                    }
                    if let Some(d) = go {
                        self.path = if self.path.is_empty() { d } else { format!("{}/{d}", self.path) };
                    }
                });
            });
    }
}

fn row(ui: &mut Ui, icon: &str, text: &str, chevron: bool) -> egui::Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 6.0, p.hover);
    }
    let label = if icon.is_empty() { text.to_string() } else { format!("{icon}   {text}") };
    ui.painter().text(rect.left_center() + vec2(12.0, 0.0), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(14.0), p.text);
    if chevron {
        ui.painter().text(rect.right_center() - vec2(12.0, 0.0), egui::Align2::RIGHT_CENTER, "›", egui::FontId::proportional(18.0), p.muted);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}
