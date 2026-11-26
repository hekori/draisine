//! Preview of a plan: the two-column "Changes" table (what happens on each side and why), the
//! "Result" list (which files exist after applying and what they contain) and the text diff.

use super::theme::{self, Pal};
use super::{Bg, human, when};
use crate::config::Config;
use crate::engine::{self, Action, Change, Item, Mode, Plan};
use crate::rclone::{Entry, Rclone, join};
use eframe::egui::{self, Color32, CursorIcon, Layout, Margin, Rect, RichText, Sense, Stroke, Ui, vec2};
use std::collections::{HashMap, HashSet};

#[derive(PartialEq, Clone, Copy)]
pub enum View {
    Changes,
    Result,
}

/// Lines of a text diff: `Old` = only in the version that gets replaced, `New` = only in the
/// version that replaces it (see `diff_sides`).
pub enum DLine {
    Same(String),
    Old(String),
    New(String),
    Skip(usize),
}

type DiffResult = Result<Vec<DLine>, String>;

pub enum DiffState {
    Loading(Bg<DiffResult>),
    Ready(DiffResult),
}

/// Loaded text diffs (for the line counts and the "Compare" panels) and which panels are open,
/// keyed by pair id + path.
#[derive(Default)]
pub struct Diffs {
    loaded: HashMap<String, DiffState>,
    open: HashSet<String>,
}

impl Diffs {
    pub fn clear(&mut self) {
        self.loaded.clear();
        self.open.clear();
    }
    fn poll(&mut self) {
        for d in self.loaded.values_mut() {
            if let DiffState::Loading(bg) = d
                && let Some(r) = bg.poll()
            {
                *d = DiffState::Ready(r);
            }
        }
    }
    /// Added / removed lines, once the diff is loaded and the files are text.
    fn counts(&self, key: &str) -> Option<(usize, usize)> {
        match self.loaded.get(key)? {
            DiffState::Ready(Ok(lines)) => Some(lines.iter().fold((0, 0), |(a, r), l| match l {
                DLine::New(_) => (a + 1, r),
                DLine::Old(_) => (a, r + 1),
                _ => (a, r),
            })),
            _ => None,
        }
    }
}

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
/// Line counts are loaded in the background for the first this many replaced files.
const MAX_COUNTED: usize = 30;

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
    /// The side where the file does not exist: no name, only a short note.
    fn gone(p: &Pal, note: impl Into<String>) -> Cell {
        Cell { marker: "", name: String::new(), meta: String::new(), note: note.into(), color: p.muted, strike: false, dim: true }
    }
    fn mark(marker: &'static str, color: Color32, name: &str, e: Option<Entry>, note: impl Into<String>) -> Cell {
        Cell { marker, name: name.into(), meta: meta(e), note: note.into(), color, strike: false, dim: false }
    }
}

/// How much a replaced file changed: "+3 −1 lines, +120 B". `lines` is `None` while the text
/// diff is loading or for binary / large files.
fn amount(it: &Item, lines: Option<(usize, usize)>) -> String {
    let (new, old) = match it.action {
        Action::OverwriteRemote => (it.local, it.remote),
        Action::OverwriteLocal => (it.remote, it.local),
        _ => return String::new(),
    };
    let mut parts = Vec::new();
    match lines {
        Some((0, 0)) => parts.push("same text".to_string()),
        Some((add, del)) => {
            let n = |c: char, k: usize| if k > 0 { format!("{c}{k} ") } else { String::new() };
            let word = if add + del == 1 { "line" } else { "lines" };
            parts.push(format!("{}{}{word}", n('+', add), n('−', del)));
        }
        None => {}
    }
    if let (Some(new), Some(old)) = (new, old) {
        let d = new.size - old.size;
        parts.push(match d {
            0 => "same size".to_string(),
            d if d > 0 => format!("+{}", human(d)),
            d => format!("−{}", human(-d)),
        });
    }
    parts.join(", ")
}

fn with(note: &str, extra: &str) -> String {
    [note, extra].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ")
}

/// (this computer, cloud). The side where something changed carries the marker and says what
/// changed and how much; the other side says what it becomes after the sync, or that it stays
/// as it is when the row is not selected.
fn cells(p: &Pal, it: &Item, mode: Mode, first_sync: bool, lines: Option<(usize, usize)>) -> (Cell, Cell) {
    let path = it.path.as_str();
    let two = mode == Mode::TwoWay;
    let sel = it.selected;
    // What changed on each side (two-way), with a fallback for one-way modes.
    let ln = |fallback: &str| if two { change_note(it.local_change, first_sync).to_string() } else { fallback.to_string() };
    let rn = |fallback: &str| if two { change_note(it.remote_change, first_sync).to_string() } else { fallback.to_string() };
    let keep = "stays as it is (not synced)";
    let howmuch = amount(it, lines);
    // The side that receives the change: dim and muted until it is selected.
    let target = |mut c: Cell| {
        if !sel {
            c.color = p.muted;
            c.marker = "";
            c.strike = false;
        }
        c
    };
    // The side a new file is copied to: no name (same as on the other side). Like the other
    // target cells it shows the current state, which is: the file does not exist there yet.
    let added = |where_: &str| {
        let note = if sel { format!("not {where_} yet") } else { "stays missing (not synced)".into() };
        Cell { dim: true, ..Cell::mark("", p.muted, "", None, note) }
    };
    match it.action {
        Action::CopyToLocal => (
            added("on this computer"),
            Cell::mark("+", p.green, path, it.remote, rn("not on this computer")),
        ),
        Action::CopyToRemote => (
            Cell::mark("+", p.green, path, it.local, ln("not in the cloud")),
            added("in the cloud"),
        ),
        Action::OverwriteLocal => (
            target(Cell::plain(p, "", it.local, if sel { "" } else { keep })),
            Cell::mark("~", p.orange, path, it.remote, with(&rn("differs from this computer"), &howmuch)),
        ),
        Action::OverwriteRemote => (
            Cell::mark("~", p.orange, path, it.local, with(&ln("differs from the cloud"), &howmuch)),
            target(Cell::plain(p, "", it.remote, if sel { "" } else { keep })),
        ),
        Action::DeleteLocal => (
            target(Cell { strike: true, ..Cell::mark("", p.red, path, it.local, if sel { "will be deleted · moves to the trash" } else { keep }) }),
            Cell::gone(p, if two { rn("") } else { "not in the cloud".into() }),
        ),
        Action::DeleteRemote => (
            Cell::gone(p, if two { ln("") } else { "not on this computer".into() }),
            target(Cell { strike: true, ..Cell::mark("", p.red, path, it.remote, if sel { "will be deleted · kept in backup / provider's trash" } else { keep }) }),
        ),
        Action::RenameLocal => {
            let from = it.from.as_deref().unwrap_or_default();
            (
                target(Cell::plain(p, from, it.local, if sel { format!("will be renamed to {path}") } else { keep.into() })),
                Cell::mark("~", p.blue, path, it.remote, format!("renamed from {from}")),
            )
        }
        Action::RenameRemote => {
            let from = it.from.as_deref().unwrap_or_default();
            (
                Cell::mark("~", p.blue, path, it.local, format!("renamed from {from}")),
                target(Cell::plain(p, from, it.remote, if sel { format!("will be renamed to {path}") } else { keep.into() })),
            )
        }
        Action::Conflict => {
            let copy = it.copy_name.as_deref().unwrap_or_default();
            let copy = copy.rsplit('/').next().unwrap_or(copy);
            let (l, r) = if sel { (format!("changed here too · kept as “{copy}”"), "changed here too · keeps the name".to_string()) } else { (keep.into(), keep.into()) };
            (Cell::mark("⚠", p.orange, path, it.local, l), Cell::mark("⚠", p.orange, path, it.remote, r))
        }
        Action::OnlyLocal => (Cell::mark("+", p.green, path, it.local, "only on this computer"), Cell::gone(p, "does not exist in the cloud")),
        Action::OnlyRemote => (Cell::gone(p, "does not exist on this computer"), Cell::mark("+", p.green, path, it.remote, "only in the cloud")),
        Action::Different => (
            Cell::mark("~", p.orange, path, it.local, "differs from the cloud"),
            Cell::mark("~", p.orange, path, it.remote, "differs from this computer"),
        ),
    }
}

fn file_job(p: &Pal, name: &str, color: Color32, strike: bool, size: f32) -> egui::text::LayoutJob {
    let (dir, file) = match name.rfind('/') {
        // Keep "a/old ➡ a/new" in one piece: split only at the last slash before any arrow.
        Some(i) if !name[..i].contains(" ➡ ") => (&name[..=i], &name[i + 1..]),
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

/// Draws a cell: marker + name on the first line, size · time · note below.
fn cell_ui(ui: &mut Ui, p: &Pal, c: &Cell, rect: Rect) {
    let center = Layout::left_to_right(egui::Align::Center);
    let top = Rect::from_min_size(rect.min, vec2(rect.width(), 22.0));
    let bottom = Rect::from_min_size(rect.min + vec2(18.0, 22.0), vec2(rect.width() - 18.0, 18.0));
    let mut u = child(ui, top, center);
    u.spacing_mut().item_spacing.x = 6.0;
    let (m, _) = u.allocate_exact_size(vec2(12.0, 18.0), Sense::hover());
    if !c.marker.is_empty() {
        u.painter().text(m.center(), egui::Align2::CENTER_CENTER, c.marker, egui::FontId::proportional(15.0), c.color);
    }
    let text = if c.dim { c.color.gamma_multiply(0.75) } else { c.color };
    u.add(egui::Label::new(file_job(p, &c.name, text, c.strike, 14.0)).truncate());
    let sub: Vec<&str> = [c.meta.as_str(), c.note.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
    if !sub.is_empty() {
        child(ui, bottom, center)
            .add(egui::Label::new(RichText::new(sub.join("  ·  ")).size(12.0).color(p.muted)).truncate());
    }
}

/// Column rectangles of one table row: checkbox | this computer | cloud | compare.
pub struct Cols {
    cb: Rect,
    l: Rect,
    r: Rect,
    cmp: Rect,
}

fn columns(row: Rect, selectable: bool) -> Cols {
    let cb_w = if selectable { 28.0 } else { 0.0 };
    let (gap, cmp_w) = (16.0, 72.0);
    let col = ((row.width() - cb_w - gap - cmp_w) / 2.0).max(80.0);
    let mut x = row.left();
    let mut take = |w: f32| {
        let r = Rect::from_min_size(egui::pos2(x, row.top()), vec2(w, row.height()));
        x += w;
        r
    };
    let (cb, l) = (take(cb_w), take(col));
    take(gap);
    Cols { cb, l, r: take(col), cmp: take(cmp_w) }
}

/// Column header: "This computer" with the local folder on the left, "Cloud" with the remote
/// folder on the right, aligned with the rows of the changes table. Returns the columns.
pub fn sides(ui: &mut Ui, p: &Pal, plan: &Plan, selectable: bool) -> Cols {
    let (hdr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 42.0), Sense::hover());
    let c = columns(hdr.shrink2(vec2(10.0, 0.0)), selectable);
    let sides = [
        (c.l, "💻  This computer", &plan.pair.local, plan.local_files),
        (c.r, "☁  Cloud", &plan.pair.remote, plan.remote_files),
    ];
    for (rect, title, path, n) in sides {
        let mut cu = child(ui, rect.shrink2(vec2(0.0, 2.0)), Layout::top_down(egui::Align::Min));
        cu.spacing_mut().item_spacing.y = 2.0;
        cu.label(theme::bold(title, 13.0).color(p.muted));
        cu.add(egui::Label::new(RichText::new(format!("{}  ·  {n} files", theme::short_path(path))).size(12.0).color(p.muted)).truncate())
            .on_hover_text(path.as_str());
    }
    c
}

fn child(ui: &mut Ui, rect: Rect, layout: Layout) -> Ui {
    ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout))
}

fn comparable(a: Action) -> bool {
    matches!(a, Action::OverwriteLocal | Action::OverwriteRemote | Action::Conflict | Action::Different)
}

/// Two-column table: this computer | cloud, one row per item.
pub fn changes(ui: &mut Ui, p: &Pal, cfg: &Config, plan: &mut Plan, mode: Mode, f: &Filter, diffs: &mut Diffs) {
    let selectable = mode != Mode::Diff;
    let idx: Vec<usize> = plan
        .items
        .iter()
        .enumerate()
        .filter(|(_, it)| !f.hides(it.action, &[&it.path, it.from.as_deref().unwrap_or("")]))
        .map(|(i, _)| i)
        .collect();
    // Header (same columns as the rows, inside the same horizontal margin)
    let c = sides(ui, p, plan, selectable);
    if idx.is_empty() {
        ui.label(theme::muted(ui, "No changes match the filter."));
        return;
    }
    let center = Layout::left_to_right(egui::Align::Center);

    if selectable {
        let n_sel = idx.iter().filter(|&&i| plan.items[i].selected).count();
        let mut all = n_sel == idx.len();
        let cb = egui::Checkbox::without_text(&mut all).indeterminate(n_sel > 0 && n_sel < idx.len());
        if child(ui, c.cb, center).add(cb).on_hover_text("Select all / none").changed() {
            for &i in &idx {
                plan.items[i].selected = all;
            }
        }
    }

    // Line counts for replaced text files, loaded in the background (the same diff "Compare" shows).
    diffs.poll();
    let small = |e: Option<Entry>| e.is_some_and(|e| (e.size as u64) <= MAX_DIFF_BYTES);
    for &i in idx.iter().filter(|&&i| matches!(plan.items[i].action, Action::OverwriteLocal | Action::OverwriteRemote)).take(MAX_COUNTED) {
        let it = &plan.items[i];
        let key = format!("{}\u{1f}{}", plan.pair.id, it.path);
        if small(it.local) && small(it.remote) && !diffs.loaded.contains_key(&key) {
            diffs.loaded.insert(key, DiffState::Loading(load_diff(ui.ctx(), cfg, plan.pair.clone(), it.path.clone(), it.action)));
        }
    }

    egui::Frame::new().fill(p.surface2).corner_radius(8).inner_margin(Margin::symmetric(10, 4)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        for (n, &i) in idx.iter().take(MAX_ROWS).enumerate() {
            let key = format!("{}\u{1f}{}", plan.pair.id, plan.items[i].path);
            let lines = diffs.counts(&key);
            let it = &mut plan.items[i];
            let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::hover());
            if n > 0 {
                ui.painter().hline(row.x_range(), row.top(), Stroke::new(1.0, p.border.gamma_multiply(0.6)));
            }
            let c = columns(row, selectable);
            if selectable {
                child(ui, c.cb, center).add(egui::Checkbox::without_text(&mut it.selected));
            }
            let (l, r) = cells(p, it, mode, plan.first_sync, lines);
            let pad = |r: Rect| Rect::from_min_max(r.min + vec2(0.0, 4.0), r.max - vec2(8.0, 4.0));
            cell_ui(ui, p, &l, pad(c.l));
            cell_ui(ui, p, &r, pad(c.r));
            if !it.reason.is_empty() {
                ui.interact(c.l.union(c.r), ui.scope_id().with(("why", &key)), Sense::hover()).on_hover_text(&it.reason);
            }
            if comparable(it.action) {
                let open = diffs.open.contains(&key);
                let label = if open { "Hide" } else { "Compare" };
                let mut cu = child(ui, c.cmp, Layout::right_to_left(egui::Align::Center));
                if theme::link(&mut cu, label).on_hover_text("Show the differences line by line (text files)").clicked() {
                    if open {
                        diffs.open.remove(&key);
                    } else {
                        diffs.open.insert(key.clone());
                        if !diffs.loaded.contains_key(&key) {
                            let bg = load_diff(ui.ctx(), cfg, plan.pair.clone(), it.path.clone(), it.action);
                            diffs.loaded.insert(key.clone(), DiffState::Loading(bg));
                        }
                    }
                }
            }
            if diffs.open.contains(&key)
                && let Some(d) = diffs.loaded.get(&key)
            {
                diff_ui(ui, p, d, &key, it.action);
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

/// Which side is "old" (−) and which is "new" (+), with labels. The diff is oriented like the
/// sync: it shows how the file that gets replaced will change. Without a direction (conflict,
/// diff mode), it reads this computer ➡ cloud.
fn diff_sides(a: Action) -> (bool, &'static str, &'static str) {
    // (old side is the cloud, old label, new label)
    match a {
        Action::OverwriteRemote => (true, "− cloud now", "+ cloud after sync (from this computer)"),
        Action::OverwriteLocal => (false, "− this computer now", "+ this computer after sync (from the cloud)"),
        _ => (false, "− this computer", "+ cloud"),
    }
}

fn load_diff(ctx: &egui::Context, cfg: &Config, pair: crate::config::Pair, path: String, action: Action) -> Bg<DiffResult> {
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
        if diff_sides(action).0 { line_diff(&remote, &local) } else { line_diff(&local, &remote) }
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
        } else if i < n && (j == m || dp[(i + 1) * w + j] >= dp[i * w + j + 1]) {
            out.push(DLine::Old(am[i].into()));
            i += 1;
        } else {
            out.push(DLine::New(bm[j].into()));
            j += 1;
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

fn diff_ui(ui: &mut Ui, p: &Pal, d: &DiffState, key: &str, action: Action) {
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
                    if !lines.iter().any(|l| matches!(l, DLine::Old(_) | DLine::New(_))) {
                        ui.label(theme::muted(ui, "The text is the same (only line endings or the end of the file differ)."));
                        return;
                    }
                    if ui.available_width() >= 560.0 {
                        split_ui(ui, p, lines, key, action);
                    } else {
                        unified_ui(ui, p, lines, key, action);
                    }
                }
            }
        });
}

/// One line on one side of the side-by-side diff.
struct Side<'a> {
    no: usize,
    text: &'a str,
    changed: bool,
}

/// A row of the side-by-side diff, oriented (old, new).
enum SRow<'a> {
    Pair(Option<Side<'a>>, Option<Side<'a>>),
    Skip(usize),
}

/// Pairs up the lines of a diff for the side-by-side view: unchanged lines sit next to each
/// other, and each run of changes puts its removed lines next to its added lines.
fn split_rows(lines: &[DLine]) -> Vec<SRow<'_>> {
    let (mut o, mut n) = (1, 1);
    let mut out = Vec::new();
    let mut k = 0;
    while k < lines.len() {
        match &lines[k] {
            DLine::Same(s) => {
                out.push(SRow::Pair(Some(Side { no: o, text: s, changed: false }), Some(Side { no: n, text: s, changed: false })));
                (o, n, k) = (o + 1, n + 1, k + 1);
            }
            DLine::Skip(c) => {
                out.push(SRow::Skip(*c));
                (o, n, k) = (o + c, n + c, k + 1);
            }
            _ => {
                let (mut olds, mut news) = (Vec::new(), Vec::new());
                while let Some(l) = lines.get(k) {
                    match l {
                        DLine::Old(s) => olds.push(s.as_str()),
                        DLine::New(s) => news.push(s.as_str()),
                        _ => break,
                    }
                    k += 1;
                }
                for r in 0..olds.len().max(news.len()) {
                    let a = olds.get(r).map(|&text| Side { no: o + r, text, changed: true });
                    let b = news.get(r).map(|&text| Side { no: n + r, text, changed: true });
                    out.push(SRow::Pair(a, b));
                }
                (o, n) = (o + olds.len(), n + news.len());
            }
        }
    }
    out
}

/// Side-by-side diff: this computer on the left, the cloud on the right (like the table above).
/// Lines that get replaced are red, lines that replace them green.
fn split_ui(ui: &mut Ui, p: &Pal, lines: &[DLine], key: &str, action: Action) {
    let old_is_cloud = diff_sides(action).0;
    let gap = 8.0;
    let half = ((ui.available_width() - gap) / 2.0).floor();
    let head = |ui: &mut Ui, title: &str| {
        ui.allocate_ui_with_layout(vec2(half, 0.0), Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_width(half);
            ui.label(RichText::new(title).size(12.0).strong());
        });
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        head(ui, "💻 This computer");
        head(ui, "☁ Cloud");
    });
    ui.add_space(4.0);

    let rows = split_rows(lines);
    egui::ScrollArea::vertical().id_salt(key).max_height(360.0).show(ui, |ui| {
        ui.spacing_mut().item_spacing = vec2(gap, 0.0);
        let mono = egui::FontId::monospace(12.5);
        let width = ui.available_width();
        let half = ((width - gap) / 2.0).floor();
        let digits = rows.iter().map(|r| match r {
            SRow::Pair(a, b) => a.iter().chain(b).map(|s| s.no).max().unwrap_or(0),
            SRow::Skip(_) => 0,
        }).max().unwrap_or(0).to_string().len();
        // (line, is it on the side that gets replaced)
        let cell = |ui: &mut Ui, s: &Option<Side>, old: bool| -> Color32 {
            let (bg, color, mark) = match s {
                Some(s) if s.changed && old => (p.red.gamma_multiply(0.12), p.red, "−"),
                Some(s) if s.changed => (p.green.gamma_multiply(0.12), p.green, "+"),
                Some(_) => (Color32::TRANSPARENT, p.text, " "),
                None => (p.muted.gamma_multiply(0.06), p.muted, " "),
            };
            ui.allocate_ui_with_layout(vec2(half, 0.0), Layout::left_to_right(egui::Align::Min), |ui| {
                ui.set_width(half);
                ui.spacing_mut().item_spacing.x = 0.0;
                let no = s.as_ref().map(|s| s.no.to_string()).unwrap_or_default();
                ui.label(RichText::new(format!("{no:>digits$} ")).font(mono.clone()).color(p.muted));
                ui.label(RichText::new(format!("{mark} ")).font(mono.clone()).color(color));
                if let Some(s) = s {
                    ui.add(egui::Label::new(RichText::new(s.text).font(mono.clone()).color(color)).wrap());
                }
            });
            bg
        };
        for row in &rows {
            match row {
                SRow::Skip(n) => {
                    let t = format!("⋯ {n} unchanged line{}", if *n == 1 { "" } else { "s" });
                    egui::Frame::new().inner_margin(Margin::symmetric(0, 2)).show(ui, |ui| {
                        ui.set_width(width);
                        ui.vertical_centered(|ui| ui.label(RichText::new(t).size(12.0).color(p.muted)));
                    });
                }
                SRow::Pair(old, new) => {
                    let (l, r, l_old) = if old_is_cloud { (new, old, false) } else { (old, new, true) };
                    let top = ui.cursor().min;
                    let (bl, br) = (ui.painter().add(egui::Shape::Noop), ui.painter().add(egui::Shape::Noop));
                    let resp = ui.horizontal_top(|ui| (cell(ui, l, l_old), cell(ui, r, !l_old)));
                    let (cl, cr) = resp.inner;
                    let h = resp.response.rect.height();
                    ui.painter().set(bl, egui::Shape::rect_filled(Rect::from_min_size(top, vec2(half, h)), 0, cl));
                    let rx = egui::pos2(top.x + half + gap, top.y);
                    ui.painter().set(br, egui::Shape::rect_filled(Rect::from_min_size(rx, vec2(half, h)), 0, cr));
                }
            }
        }
    });
}

/// Unified diff (one column, − / + lines), for narrow windows.
fn unified_ui(ui: &mut Ui, p: &Pal, lines: &[DLine], key: &str, action: Action) {
    ui.horizontal(|ui| {
        let (_, old, new) = diff_sides(action);
        ui.label(RichText::new(old).size(12.0).color(p.red));
        ui.label(RichText::new(new).size(12.0).color(p.green));
    });
    egui::ScrollArea::vertical().id_salt(key).max_height(320.0).show(ui, |ui| {
        let mono = egui::FontId::monospace(12.5);
        for l in lines.iter() {
            let (t, c, bg) = match l {
                DLine::Same(s) => (format!("  {s}"), p.text, Color32::TRANSPARENT),
                DLine::Old(s) => (format!("− {s}"), p.red, p.red.gamma_multiply(0.10)),
                DLine::New(s) => (format!("+ {s}"), p.green, p.green.gamma_multiply(0.10)),
                DLine::Skip(n) => (format!("  ⋯ {n} unchanged lines"), p.muted, Color32::TRANSPARENT),
            };
            egui::Frame::new().fill(bg).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(egui::Label::new(RichText::new(t).font(mono.clone()).color(c)).wrap());
            });
        }
    });
}

/// Makes a summary chip clickable (filters both views by that kind of change).
pub fn chip(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, text: &str, color: Color32, active: Option<bool>) -> egui::Response {
    let color = if active == Some(false) { color.gamma_multiply(0.45) } else { color };
    let r = theme::badge(ui, text, color);
    ui.interact(r.rect, ui.scope_id().with(("chip", id)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(d: &[DLine]) -> String {
        d.iter()
            .map(|l| match l {
                DLine::Same(s) => format!(" {s}"),
                DLine::Old(s) => format!("-{s}"),
                DLine::New(s) => format!("+{s}"),
                DLine::Skip(n) => format!("~{n}"),
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    #[test]
    fn change_amount() {
        let e = |size| Some(Entry { size, mtime_ms: 0 });
        let it = Item {
            path: "cheese.txt".into(),
            action: Action::OverwriteRemote,
            reason: String::new(),
            local: e(5),
            remote: e(0),
            local_change: Change::Modified,
            remote_change: Change::Unchanged,
            from: None,
            copy_name: None,
            selected: true,
        };
        assert_eq!(amount(&it, Some((1, 0))), "+1 line, +5 B");
        assert_eq!(amount(&it, None), "+5 B");
        let it = Item { action: Action::OverwriteLocal, ..it };
        assert_eq!(amount(&it, Some((2, 3))), "+2 −3 lines, −5 B");
    }

    #[test]
    fn diff_lines() {
        let d = line_diff("a\nb\nc\n", "a\nB\nc\nd\n").unwrap();
        assert_eq!(show(&d), " a|-b|+B| c|+d");
        let long: String = (0..20).map(|i| format!("{i}\n")).collect();
        let changed = long.replace("10\n", "ten\n");
        assert_eq!(show(&line_diff(&long, &changed).unwrap()), "~7| 7| 8| 9|-10|+ten| 11| 12| 13|~6");
    }

    #[test]
    fn side_by_side_rows() {
        let d = line_diff("a\nb\nc\n", "a\nB\nB2\nc\n").unwrap();
        let side = |s: &Option<Side>| s.as_ref().map(|s| format!("{}{}{}", s.no, if s.changed { "*" } else { "" }, s.text)).unwrap_or("-".into());
        let rows: Vec<String> = split_rows(&d)
            .iter()
            .map(|r| match r {
                SRow::Pair(a, b) => format!("{}/{}", side(a), side(b)),
                SRow::Skip(n) => format!("~{n}"),
            })
            .collect();
        assert_eq!(rows.join("|"), "1a/1a|2*b/2*B|-/3*B2|3c/4c");
        let long: String = (0..20).map(|i| format!("{i}\n")).collect();
        let d = line_diff(&long, &long.replace("10\n", "")).unwrap();
        match &split_rows(&d)[..2] {
            [SRow::Skip(7), SRow::Pair(Some(a), Some(b))] => assert_eq!((a.no, b.no), (8, 8)),
            _ => panic!(),
        }
    }
}
