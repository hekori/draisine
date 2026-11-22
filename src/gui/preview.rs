//! Preview of a plan: the two-column "Changes" table (what happens on each side and why), the
//! "Result" list (which files exist after applying and what they contain) and the text diff.

use super::theme::{self, Pal};
use super::{Bg, action_color, human, when};
use crate::config::Config;
use crate::engine::{self, Action, Change, Item, Mode, Plan};
use crate::rclone::{Entry, Rclone, join};
use eframe::egui::{self, Color32, CursorIcon, Layout, Margin, RichText, Sense, Stroke, Ui, vec2};
use std::collections::HashMap;

#[derive(PartialEq, Clone, Copy)]
pub enum View {
    Changes,
    Result,
}

/// Lines of a text diff between the local and the cloud version.
pub enum DLine {
    Same(String),
    Local(String),
    Remote(String),
    Skip(usize),
}

type DiffResult = Result<Vec<DLine>, String>;

pub enum DiffState {
    Loading(Bg<DiffResult>),
    Ready(DiffResult),
}

/// Open "Compare" panels, keyed by pair id + path.
pub type Diffs = HashMap<String, DiffState>;

/// Shared filters of both views.
pub struct Filter<'a> {
    pub text: &'a str,
    pub only: Option<Action>,
}

impl Filter<'_> {
    fn hides(&self, action: Action, names: &[&str]) -> bool {
        self.only.is_some_and(|a| a != action)
            || (!self.text.is_empty() && !names.iter().any(|n| n.to_lowercase().contains(self.text)))
    }
}

const MAX_ROWS: usize = 300;
const MAX_DIFF_BYTES: u64 = 1024 * 1024;

fn meta(e: Option<Entry>) -> String {
    e.map(|e| format!("{} · {}", human(e.size), when(&e))).unwrap_or_default()
}

fn change_note(c: Change, first_sync: bool) -> &'static str {
    match c {
        Change::New if first_sync => "only here",
        Change::New => "new since last sync",
        Change::Modified => "changed since last sync",
        Change::Deleted => "deleted since last sync",
        Change::Unchanged => "unchanged since last sync",
        Change::Absent => "",
    }
}

// ------------------------------------------------------------------ changes view

/// One side (cell) of a row in the changes table.
struct Cell {
    marker: &'static str,
    name: String,
    meta: String,
    note: String,
    color: Color32,
    strike: bool,
    dim: bool,
}

impl Cell {
    fn plain(p: &Pal, name: &str, e: Option<Entry>, note: impl Into<String>) -> Cell {
        Cell { marker: "", name: name.into(), meta: meta(e), note: note.into(), color: p.text, strike: false, dim: false }
    }
    fn gone(p: &Pal, name: &str, note: impl Into<String>) -> Cell {
        Cell { marker: "", name: name.into(), meta: String::new(), note: note.into(), color: p.muted, strike: true, dim: true }
    }
    fn mark(marker: &'static str, color: Color32, name: &str, e: Option<Entry>, note: impl Into<String>) -> Cell {
        Cell { marker, name: name.into(), meta: meta(e), note: note.into(), color, strike: false, dim: false }
    }
}

/// (this computer, arrow, cloud)
fn cells(p: &Pal, it: &Item, mode: Mode, first_sync: bool) -> (Cell, &'static str, Cell) {
    let path = it.path.as_str();
    let two = mode == Mode::TwoWay;
    let ln = if two { change_note(it.local_change, first_sync) } else { "" };
    let rn = if two { change_note(it.remote_change, first_sync) } else { "" };
    match it.action {
        Action::CopyToLocal => (
            Cell { dim: true, ..Cell::mark("+", p.green, path, it.remote, "will be downloaded") },
            "⬅",
            Cell::plain(p, path, it.remote, rn),
        ),
        Action::CopyToRemote => (
            Cell::plain(p, path, it.local, ln),
            "➡",
            Cell { dim: true, ..Cell::mark("+", p.green, path, it.local, "will be uploaded") },
        ),
        Action::OverwriteLocal => (
            Cell::mark("~", p.orange, path, it.local, "replaced by the cloud version, old one → trash"),
            "⬅",
            Cell::plain(p, path, it.remote, rn),
        ),
        Action::OverwriteRemote => (
            Cell::plain(p, path, it.local, ln),
            "➡",
            Cell::mark("~", p.orange, path, it.remote, "replaced, old one → backup / version history"),
        ),
        Action::DeleteLocal => (
            Cell { strike: true, ..Cell::mark("−", p.red, path, it.local, "moves to the trash") },
            "⬅",
            Cell::gone(p, path, if two { rn } else { "not in the cloud" }),
        ),
        Action::DeleteRemote => (
            Cell::gone(p, path, if two { ln } else { "not on this computer" }),
            "➡",
            Cell { strike: true, ..Cell::mark("−", p.red, path, it.remote, "goes to the backup folder / provider's trash") },
        ),
        Action::RenameLocal => {
            let from = it.from.as_deref().unwrap_or_default();
            (
                Cell::mark("~", p.blue, &format!("{from} → {path}"), it.local, "will be renamed"),
                "⬅",
                Cell::plain(p, path, it.remote, format!("renamed from {from}")),
            )
        }
        Action::RenameRemote => {
            let from = it.from.as_deref().unwrap_or_default();
            (
                Cell::plain(p, path, it.local, format!("renamed from {from}")),
                "➡",
                Cell::mark("~", p.blue, &format!("{from} → {path}"), it.remote, "will be renamed"),
            )
        }
        Action::Conflict => {
            let copy = it.copy_name.as_deref().unwrap_or_default();
            let copy = copy.rsplit('/').next().unwrap_or(copy);
            (
                Cell::mark("⚠", p.orange, path, it.local, format!("{ln} · kept as “{copy}”")),
                "⇄",
                Cell::mark("⚠", p.orange, path, it.remote, format!("{rn} · keeps the name")),
            )
        }
        Action::Different => (
            Cell::mark("≠", p.orange, path, it.local, ""),
            "",
            Cell::mark("≠", p.orange, path, it.remote, ""),
        ),
        Action::OnlyLocal => (Cell::plain(p, path, it.local, ""), "", Cell::gone(p, path, "not in the cloud")),
        Action::OnlyRemote => (Cell::gone(p, path, "not on this computer"), "", Cell::plain(p, path, it.remote, "")),
    }
}

fn file_job(p: &Pal, name: &str, color: Color32, strike: bool, size: f32) -> egui::text::LayoutJob {
    let (dir, file) = match name.rfind('/') {
        // Keep "a/old → a/new" in one piece: split only at the last slash before any arrow.
        Some(i) if !name[..i].contains(" → ") => (&name[..=i], &name[i + 1..]),
        _ => ("", name),
    };
    let mut job = egui::text::LayoutJob::default();
    let f = egui::FontId::proportional(size);
    let strike = if strike { Stroke::new(1.0, color) } else { Stroke::NONE };
    let fmt = |c: Color32| egui::TextFormat { font_id: f.clone(), color: c, strikethrough: strike, ..Default::default() };
    job.append(dir, 0.0, fmt(p.muted));
    job.append(file, 0.0, fmt(color));
    job
}

fn cell_ui(ui: &mut Ui, p: &Pal, c: &Cell, width: f32) {
    ui.allocate_ui_with_layout(vec2(width, 38.0), Layout::top_down(egui::Align::Min), |ui| {
        ui.set_width(width);
        ui.spacing_mut().item_spacing.y = 1.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let (rect, _) = ui.allocate_exact_size(vec2(12.0, 16.0), Sense::hover());
            if !c.marker.is_empty() {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    c.marker,
                    egui::FontId::proportional(15.0),
                    c.color,
                );
            }
            let text = if c.marker.is_empty() && !c.dim { p.text } else { c.color };
            let text = if c.dim { text.gamma_multiply(0.75) } else { text };
            ui.add(egui::Label::new(file_job(p, &c.name, text, c.strike, 14.0)).truncate());
        });
        let sub: Vec<&str> = [c.meta.as_str(), c.note.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
        if !sub.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                ui.add(egui::Label::new(RichText::new(sub.join("  ·  ")).size(12.0).color(p.muted)).truncate());
            });
        }
    });
}

fn comparable(a: Action) -> bool {
    matches!(a, Action::OverwriteLocal | Action::OverwriteRemote | Action::Conflict | Action::Different)
}

/// Two-column table: this computer | arrow | cloud, one row per item.
pub fn changes(ui: &mut Ui, p: &Pal, cfg: &Config, plan: &mut Plan, mode: Mode, f: &Filter, diffs: &mut Diffs) {
    let selectable = mode != Mode::Diff;
    let idx: Vec<usize> = plan
        .items
        .iter()
        .enumerate()
        .filter(|(_, it)| !f.hides(it.action, &[&it.path, it.from.as_deref().unwrap_or("")]))
        .map(|(i, _)| i)
        .collect();
    if idx.is_empty() {
        ui.label(theme::muted(ui, "No changes match the filter."));
        return;
    }
    let (cb_w, arrow_w, cmp_w) = (if selectable { 28.0 } else { 0.0 }, 30.0, 74.0);
    let col = |ui: &Ui| ((ui.available_width() - cb_w - arrow_w - cmp_w - 24.0) / 2.0).max(120.0);

    // Header
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        if selectable {
            let n_sel = idx.iter().filter(|&&i| plan.items[i].selected).count();
            let mut all = n_sel == idx.len();
            let cb = egui::Checkbox::without_text(&mut all).indeterminate(n_sel > 0 && n_sel < idx.len());
            if ui.add(cb).on_hover_text("Select all / none").changed() {
                for &i in &idx {
                    plan.items[i].selected = all;
                }
            }
        }
        let w = col(ui);
        for (k, t) in ["💻  This computer", "☁  Cloud"].into_iter().enumerate() {
            ui.allocate_ui_with_layout(vec2(w, 20.0), Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_width(w);
                ui.label(theme::bold(t, 13.0).color(p.muted));
            });
            if k == 0 {
                ui.add_space(arrow_w);
            }
        }
    });

    egui::Frame::new().fill(p.surface2).corner_radius(8).inner_margin(Margin::symmetric(10, 4)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        for (n, &i) in idx.iter().take(MAX_ROWS).enumerate() {
            if n > 0 {
                let r = ui.available_rect_before_wrap();
                ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, p.border.gamma_multiply(0.6)));
            }
            let key = format!("{}\u{1f}{}", plan.pair.id, plan.items[i].path);
            let it = &mut plan.items[i];
            let (l, arrow, r) = cells(p, it, mode, plan.first_sync);
            ui.horizontal(|ui| {
                ui.set_min_height(42.0);
                if selectable {
                    ui.add(egui::Checkbox::without_text(&mut it.selected));
                }
                let w = col(ui);
                cell_ui(ui, p, &l, w);
                let (rect, resp) = ui.allocate_exact_size(vec2(arrow_w, 38.0), Sense::hover());
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, arrow, egui::FontId::proportional(17.0), action_color(p, it.action));
                if !it.reason.is_empty() {
                    resp.on_hover_text(&it.reason);
                }
                cell_ui(ui, p, &r, w);
                if comparable(it.action) {
                    let open = diffs.contains_key(&key);
                    let label = if open { "Hide" } else { "Compare" };
                    if theme::link(ui, label).on_hover_text("Show the differences line by line (text files)").clicked() {
                        if open {
                            diffs.remove(&key);
                        } else {
                            diffs.insert(key.clone(), DiffState::Loading(load_diff(ui.ctx(), cfg, plan.pair.clone(), it.path.clone())));
                        }
                    }
                }
            });
            if let Some(d) = diffs.get_mut(&key) {
                diff_ui(ui, p, d, &key);
                ui.add_space(6.0);
            }
        }
        if idx.len() > MAX_ROWS {
            ui.label(theme::muted(ui, format!("… and {} more (use the filter)", idx.len() - MAX_ROWS)));
        }
    });
}

// ------------------------------------------------------------------ result view

struct ResRow {
    name: String,
    action: Action,
    on_local: bool,
    on_remote: bool,
    removed: bool,
    what: String,
    color: Color32,
}

fn result_rows(p: &Pal, plan: &Plan) -> Vec<ResRow> {
    let mut rows = Vec::new();
    let mut row = |name: &str, a, l, r, what: String, color| {
        rows.push(ResRow { name: name.into(), action: a, on_local: l, on_remote: r, removed: !l && !r, what, color })
    };
    let diff = plan.mode == Mode::Diff;
    for it in &plan.items {
        let path = it.path.as_str();
        let a = it.action;
        let sel = it.selected && a.executable() && !diff;
        let (lm, rm) = (meta(it.local), meta(it.remote));
        let mine = format!("your version · {lm}");
        let cloud = format!("cloud version · {rm}");
        let differs = || format!("different on each side:  💻 {lm}   ☁ {rm}");
        match (a, sel) {
            (Action::CopyToLocal, true) => row(path, a, true, true, format!("from the cloud · {rm}"), p.green),
            (Action::CopyToRemote, true) => row(path, a, true, true, format!("from this computer · {lm}"), p.green),
            (Action::OverwriteLocal, true) => row(path, a, true, true, cloud, p.orange),
            (Action::OverwriteRemote, true) => row(path, a, true, true, mine, p.orange),
            (Action::DeleteLocal, true) => row(path, a, false, false, "removed (moved to the trash)".into(), p.red),
            (Action::DeleteRemote, true) => {
                row(path, a, false, false, "removed (backup folder / provider's trash)".into(), p.red)
            }
            (Action::RenameLocal | Action::RenameRemote, true) => {
                let from = it.from.as_deref().unwrap_or_default();
                let e = if a == Action::RenameLocal { &rm } else { &lm };
                row(path, a, true, true, format!("renamed from {from} · {e}"), p.blue);
                row(from, a, false, false, format!("renamed to {path}"), p.muted);
            }
            (Action::RenameLocal | Action::RenameRemote, false) => {
                let from = it.from.as_deref().unwrap_or_default();
                let (l, r) = (a == Action::RenameLocal, a == Action::RenameRemote);
                row(from, a, l, r, format!("only {} · {}", if l { "on this computer" } else { "in the cloud" }, if l { &lm } else { &rm }), p.muted);
                row(path, a, r, l, format!("only {} · {}", if r { "on this computer" } else { "in the cloud" }, if r { &lm } else { &rm }), p.muted);
            }
            (Action::Conflict, true) => {
                row(path, a, true, true, cloud, p.orange);
                let copy = it.copy_name.as_deref().unwrap_or_default();
                row(copy, a, true, true, format!("your version of {} · {lm}", path.rsplit('/').next().unwrap_or(path)), p.orange);
            }
            _ => match (it.local.is_some(), it.remote.is_some()) {
                (true, true) => row(path, a, true, true, differs(), p.orange),
                (true, false) => row(path, a, true, false, format!("only on this computer · {lm}"), p.muted),
                (false, true) => row(path, a, false, true, format!("only in the cloud · {rm}"), p.muted),
                (false, false) => {}
            },
        }
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// The files after applying the selected changes and where their content comes from.
pub fn result(ui: &mut Ui, p: &Pal, plan: &Plan, f: &Filter, show_same: &mut bool) {
    let rows: Vec<ResRow> = result_rows(p, plan).into_iter().filter(|r| !f.hides(r.action, &[&r.name])).collect();
    let same: Vec<&String> =
        if f.only.is_some() { Vec::new() } else { plan.in_sync.iter().filter(|s| !f.hides(Action::Different, &[s])).collect() };
    ui.label(theme::muted(ui, "After “Apply”, with the changes selected now:").small());
    ui.add_space(2.0);
    egui::Frame::new().fill(p.surface2).corner_radius(8).inner_margin(Margin::symmetric(10, 6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let name_w = (ui.available_width() * 0.42).max(160.0);
        let line = |ui: &mut Ui, name: &str, color: Color32, strike: bool, l: bool, r: bool, what: &str, what_color: Color32| {
            ui.horizontal(|ui| {
                ui.set_min_height(26.0);
                ui.allocate_ui_with_layout(vec2(name_w, 22.0), Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.set_width(name_w);
                    ui.add(egui::Label::new(file_job(p, name, color, strike, 14.0)).truncate());
                });
                let on = |b: bool, s: &str| RichText::new(s).size(13.0).color(if b { p.text } else { p.muted.gamma_multiply(0.35) });
                ui.label(on(l, "💻")).on_hover_text(if l { "on this computer" } else { "not on this computer" });
                ui.label(on(r, "☁")).on_hover_text(if r { "in the cloud" } else { "not in the cloud" });
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(what).size(12.5).color(what_color)).truncate());
            });
        };
        for r in rows.iter().take(MAX_ROWS) {
            let c = if r.removed { p.muted } else { p.text };
            line(ui, &r.name, c, r.removed, r.on_local, r.on_remote, &r.what, r.color);
        }
        if rows.len() > MAX_ROWS {
            ui.label(theme::muted(ui, format!("… and {} more (use the filter)", rows.len() - MAX_ROWS)));
        }
        if !same.is_empty() {
            ui.horizontal(|ui| {
                ui.label(theme::muted(ui, format!("{} files unchanged on both sides", same.len())));
                if theme::link(ui, if *show_same { "hide" } else { "show" }).clicked() {
                    *show_same = !*show_same;
                }
            });
            if *show_same {
                for s in same.iter().take(MAX_ROWS) {
                    line(ui, s, p.text, false, true, true, "unchanged", p.muted);
                }
            }
        }
    });
}

// ------------------------------------------------------------------ text diff

fn load_diff(ctx: &egui::Context, cfg: &Config, pair: crate::config::Pair, path: String) -> Bg<DiffResult> {
    let cfg = cfg.clone();
    Bg::spawn(ctx, move || {
        let lp = std::path::Path::new(&engine::expand_home(&pair.local)).join(&path);
        let size = std::fs::metadata(&lp).map_err(|e| format!("{}: {e}", lp.display()))?.len();
        if size > MAX_DIFF_BYTES {
            return Err("File too large to compare (more than 1 MB).".into());
        }
        let local = std::fs::read(&lp).map_err(|e| format!("{}: {e}", lp.display()))?;
        let local = String::from_utf8(local).map_err(|_| "Not a text file.".to_string())?;
        let rc = Rclone::new(&cfg).map_err(|e| e.to_string())?;
        let remote = rc
            .cat(&join(&pair.remote, &path), MAX_DIFF_BYTES)
            .map_err(|e| e.to_string())?
            .ok_or("File too large to compare (more than 1 MB).")?;
        if local.contains('\0') || remote.contains('\0') || remote.contains('\u{fffd}') {
            return Err("Not a text file.".into());
        }
        line_diff(&local, &remote)
    })
}

/// Line diff (longest common subsequence) with 3 lines of context around changes.
pub fn line_diff(a: &str, b: &str) -> DiffResult {
    let a: Vec<&str> = a.lines().collect();
    let b: Vec<&str> = b.lines().collect();
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let (n, m) = (am.len(), bm.len());
    if n * m > 4_000_000 {
        return Err("Too many differences to show.".into());
    }
    // dp[i][j]: LCS length of am[i..] and bm[j..]
    let w = m + 1;
    let mut dp = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * w + j] = if am[i] == bm[j] { dp[(i + 1) * w + j + 1] + 1 } else { dp[(i + 1) * w + j].max(dp[i * w + j + 1]) };
        }
    }
    let mut out: Vec<DLine> = a[..pre].iter().map(|s| DLine::Same(s.to_string())).collect();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && am[i] == bm[j] {
            out.push(DLine::Same(am[i].into()));
            i += 1;
            j += 1;
        } else if j < m && (i == n || dp[i * w + j + 1] >= dp[(i + 1) * w + j]) {
            out.push(DLine::Remote(bm[j].into()));
            j += 1;
        } else {
            out.push(DLine::Local(am[i].into()));
            i += 1;
        }
    }
    out.extend(a[a.len() - suf..].iter().map(|s| DLine::Same(s.to_string())));

    // Collapse long unchanged runs.
    const CTX: usize = 3;
    let changed: Vec<bool> = out.iter().map(|l| !matches!(l, DLine::Same(_))).collect();
    let len = out.len();
    let near = |k: usize| (k.saturating_sub(CTX)..(k + CTX + 1).min(len)).any(|x| changed[x]);
    let mut res = Vec::new();
    let mut skipped = 0;
    for (k, l) in out.into_iter().enumerate() {
        if matches!(l, DLine::Same(_)) && !near(k) {
            skipped += 1;
            continue;
        }
        if skipped > 0 {
            res.push(DLine::Skip(skipped));
            skipped = 0;
        }
        res.push(l);
    }
    if skipped > 0 {
        res.push(DLine::Skip(skipped));
    }
    Ok(res)
}

fn diff_ui(ui: &mut Ui, p: &Pal, d: &mut DiffState, key: &str) {
    if let DiffState::Loading(bg) = d
        && let Some(r) = bg.poll()
    {
        *d = DiffState::Ready(r);
    }
    egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(8)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            match d {
                DiffState::Loading(_) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(theme::muted(ui, "Loading both versions …"));
                    });
                }
                DiffState::Ready(Err(e)) => {
                    ui.label(theme::muted(ui, e.as_str()));
                }
                DiffState::Ready(Ok(lines)) => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("− this computer").size(12.0).color(p.red));
                        ui.label(RichText::new("+ cloud").size(12.0).color(p.green));
                    });
                    if !lines.iter().any(|l| matches!(l, DLine::Local(_) | DLine::Remote(_))) {
                        ui.label(theme::muted(ui, "The text is the same (only line endings or the end of the file differ)."));
                        return;
                    }
                    egui::ScrollArea::vertical().id_salt(key).max_height(320.0).show(ui, |ui| {
                        let mono = egui::FontId::monospace(12.5);
                        for l in lines.iter() {
                            let (t, c, bg) = match l {
                                DLine::Same(s) => (format!("  {s}"), p.text, Color32::TRANSPARENT),
                                DLine::Local(s) => (format!("− {s}"), p.red, p.red.gamma_multiply(0.10)),
                                DLine::Remote(s) => (format!("+ {s}"), p.green, p.green.gamma_multiply(0.10)),
                                DLine::Skip(n) => (format!("  ⋯ {n} unchanged lines"), p.muted, Color32::TRANSPARENT),
                            };
                            egui::Frame::new().fill(bg).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.add(egui::Label::new(RichText::new(t).font(mono.clone()).color(c)).wrap());
                            });
                        }
                    });
                }
            }
        });
}

/// Makes a summary chip clickable (filters both views by that kind of change).
pub fn chip(ui: &mut Ui, text: &str, color: Color32, active: Option<bool>) -> egui::Response {
    let color = if active == Some(false) { color.gamma_multiply(0.45) } else { color };
    let r = theme::badge(ui, text, color);
    ui.interact(r.rect, ui.scope_id().with(text), Sense::click()).on_hover_cursor(CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(d: &[DLine]) -> String {
        d.iter()
            .map(|l| match l {
                DLine::Same(s) => format!(" {s}"),
                DLine::Local(s) => format!("-{s}"),
                DLine::Remote(s) => format!("+{s}"),
                DLine::Skip(n) => format!("~{n}"),
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    #[test]
    fn diff_lines() {
        let d = line_diff("a\nb\nc\n", "a\nB\nc\nd\n").unwrap();
        assert_eq!(show(&d), " a|-b|+B| c|+d");
        let long: String = (0..20).map(|i| format!("{i}\n")).collect();
        let changed = long.replace("10\n", "ten\n");
        assert_eq!(show(&line_diff(&long, &changed).unwrap()), "~7| 7| 8| 9|-10|+ten| 11| 12| 13|~6");
    }
}
