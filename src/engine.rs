//! Sync engine. rclone is only used as a transport (list / check / copy / delete); the decision
//! what to do with each file is made here, so the user can confirm every delete and overwrite.
//!
//! Every action is planned first ([`plan`]), shown to the user, and only then executed
//! ([`execute`]). Before executing, the plan is computed again and compared: if anything changed
//! in the meantime the execution is aborted.
//!
//! Two-way sync compares each side with its own listing from the last successful sync
//! (stored in the state file), so it knows which side changed. Files changed on both sides are
//! conflicts: both versions are kept as `name-YYYY-MM-DDTHH-MM-SSZ-local.ext` and
//! `name-YYYY-MM-DDTHH-MM-SSZ-remote.ext` on both sides.

use crate::config::{Config, Pair, dirs};
use crate::log;
use crate::rclone::{Cancel, Entry, Listing, Rclone, join};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Mode {
    Diff,
    Pull,
    Push,
    TwoWay,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Diff => "Diff lokal ↔ remote",
            Mode::Pull => "1-Weg-Sync remote ➡ lokal",
            Mode::Push => "1-Weg-Sync lokal ➡ remote",
            Mode::TwoWay => "2-Wege-Sync lokal ↔ remote",
        }
    }
    pub fn arg(self) -> &'static str {
        match self {
            Mode::Diff => "diff",
            Mode::Pull => "pull",
            Mode::Push => "push",
            Mode::TwoWay => "twoway",
        }
    }
    pub fn from_arg(s: &str) -> Option<Mode> {
        [Mode::Diff, Mode::Pull, Mode::Push, Mode::TwoWay].into_iter().find(|m| m.arg() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Copy to remote, the file does not exist there.
    CopyToRemote,
    /// Copy to local, the file does not exist there.
    CopyToLocal,
    /// Replace the remote file (old version: remote backup dir or provider versions/trash).
    OverwriteRemote,
    /// Replace the local file (old version goes to the trash).
    OverwriteLocal,
    DeleteRemote,
    /// Move the local file to the trash.
    DeleteLocal,
    /// Changed on both sides: keep both versions as conflict files on both sides.
    Conflict,
    // Diff only:
    OnlyLocal,
    OnlyRemote,
    Different,
}

impl Action {
    pub fn destructive(self) -> bool {
        matches!(self, Action::OverwriteRemote | Action::OverwriteLocal | Action::DeleteRemote | Action::DeleteLocal)
    }
    pub fn executable(self) -> bool {
        !matches!(self, Action::OnlyLocal | Action::OnlyRemote | Action::Different)
    }
    pub fn label(self) -> &'static str {
        match self {
            Action::CopyToRemote => "➡ hochladen",
            Action::CopyToLocal => "⬅ herunterladen",
            Action::OverwriteRemote => "➡ remote überschreiben",
            Action::OverwriteLocal => "⬅ lokal überschreiben",
            Action::DeleteRemote => "✖ remote löschen",
            Action::DeleteLocal => "✖ lokal löschen (Papierkorb)",
            Action::Conflict => "⚠ Konflikt: beide behalten",
            Action::OnlyLocal => "nur lokal",
            Action::OnlyRemote => "nur remote",
            Action::Different => "unterschiedlich",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub path: String,
    pub action: Action,
    pub reason: String,
    pub local: Option<Entry>,
    pub remote: Option<Entry>,
    pub selected: bool,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub pair: Pair,
    pub mode: Mode,
    pub items: Vec<Item>,
    /// Paths that are identical on both sides (or deleted on both sides).
    pub in_sync: Vec<String>,
    pub local_files: usize,
    pub remote_files: usize,
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn count(&self, f: impl Fn(&Item) -> bool) -> usize {
        self.items.iter().filter(|i| f(i)).count()
    }
}

// ---------------------------------------------------------------- state

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct StateEntry {
    pub l: Option<Entry>,
    pub r: Option<Entry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub signature: String,
    pub files: BTreeMap<String, StateEntry>,
    pub last_sync: Option<String>,
    pub last_result: String,
}

fn state_path(pair: &Pair) -> PathBuf {
    let id: String = pair.id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    dirs().state_dir.join(format!("{id}.json"))
}

pub fn load_state(pair: &Pair) -> State {
    let st: State = std::fs::read_to_string(state_path(pair))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if st.signature != pair.signature() {
        return State { signature: pair.signature(), ..Default::default() };
    }
    st
}

fn save_state(pair: &Pair, st: &State) -> Result<()> {
    let p = state_path(pair);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(st)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

/// Forget the last sync: the next two-way sync behaves like a first sync (nothing is deleted).
pub fn reset_state(pair: &Pair) {
    let _ = std::fs::remove_file(state_path(pair));
    log::info(format!("[{}] Sync-Status zurückgesetzt", pair.label()));
}

// ---------------------------------------------------------------- lock

/// Only one process may change files at a time (tray auto-sync vs. window).
pub struct SyncLock(#[allow(dead_code)] std::fs::File);

pub fn lock(wait_secs: u64) -> Result<SyncLock> {
    let p = dirs().runtime_dir.join("sync.lock");
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&p)?;
    let start = std::time::Instant::now();
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(SyncLock(f)),
            Err(std::fs::TryLockError::WouldBlock) => {
                if start.elapsed().as_secs() >= wait_secs {
                    bail!("Ein anderer Sync läuft gerade. Bitte später erneut versuchen.");
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

// ---------------------------------------------------------------- helpers

pub fn expand_home(p: &str) -> String {
    let p = p.trim();
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\"))
        && let Some(h) = directories::UserDirs::new() {
            return h.home_dir().join(rest).display().to_string();
        }
    p.to_string()
}

fn local_root(pair: &Pair) -> String {
    expand_home(&pair.local)
}

fn local_path(pair: &Pair, rel: &str) -> PathBuf {
    Path::new(&local_root(pair)).join(rel)
}

/// `dir/name.ext` -> `dir/name-<ts>-<side>.ext`
pub fn conflict_name(path: &str, ts: &str, side: &str) -> String {
    let (dir, file) = match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    };
    let (stem, ext) = match file.rfind('.') {
        Some(i) if i > 0 && i + 1 < file.len() => (&file[..i], &file[i..]),
        _ => (file, ""),
    };
    format!("{dir}{stem}-{ts}-{side}{ext}")
}

pub fn timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string()
}

struct Sides {
    local: Listing,
    remote: Listing,
    local_missing: bool,
    remote_missing: bool,
}

fn list_sides(rc: &Rclone, pair: &Pair) -> Result<Sides> {
    let l = rc.list(&local_root(pair), &pair.excludes)?;
    let r = rc.list(&pair.remote, &pair.excludes)?;
    Ok(Sides {
        local_missing: l.is_none(),
        remote_missing: r.is_none(),
        local: l.unwrap_or_default(),
        remote: r.unwrap_or_default(),
    })
}

/// Of the paths present on both sides, returns those with identical content.
/// Different size: different. Both sides unchanged since the last verified sync: identical.
/// Everything else is compared by content (hash) with rclone check. Size + mtime alone is not
/// trusted, two different files can have both in common.
fn identical(rc: &Rclone, cfg: &Config, pair: &Pair, s: &Sides, st: &State, paths: &[String]) -> Result<HashSet<String>> {
    let mut same = HashSet::new();
    let mut check = Vec::new();
    for p in paths {
        let (l, r) = (s.local[p], s.remote[p]);
        if l.size != r.size {
            continue;
        }
        match st.files.get(p) {
            Some(e) if e.l == Some(l) && e.r == Some(r) => {
                same.insert(p.clone());
            }
            _ => check.push(p.clone()),
        }
    }
    same.extend(rc.identical(&local_root(pair), &pair.remote, &check, cfg.check_download)?);
    Ok(same)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Ch {
    Absent,
    New,
    Unchanged,
    Modified,
    Deleted,
}

fn change(cur: Option<&Entry>, prev: Option<Entry>) -> Ch {
    match (cur, prev) {
        (None, None) => Ch::Absent,
        (Some(_), None) => Ch::New,
        (None, Some(_)) => Ch::Deleted,
        (Some(a), Some(b)) if *a == b => Ch::Unchanged,
        _ => Ch::Modified,
    }
}

// ---------------------------------------------------------------- planning

pub fn plan(cfg: &Config, pair: &Pair, mode: Mode) -> Result<Plan> {
    plan_with(cfg, pair, mode, None)
}

/// Like [`plan`], but stops (killing the running rclone) as soon as `cancel` is set.
pub fn plan_with(cfg: &Config, pair: &Pair, mode: Mode, cancel: Option<Cancel>) -> Result<Plan> {
    if pair.local.trim().is_empty() || pair.remote.trim().is_empty() {
        bail!("[{}] lokaler Ordner und Remote müssen gesetzt sein", pair.label());
    }
    let mut rc = Rclone::new(cfg)?;
    if let Some(c) = cancel {
        rc = rc.with_cancel(c);
    }
    let s = list_sides(&rc, pair)?;
    let st = load_state(pair);
    let mut warnings = Vec::new();

    // Protection against an unmounted / wrong / emptied side: never treat that as "delete all".
    let had_l = st.files.values().filter(|e| e.l.is_some()).count();
    let had_r = st.files.values().filter(|e| e.r.is_some()).count();
    for (empty, missing, had, side) in [
        (s.local.is_empty(), s.local_missing, had_l, "Lokaler Ordner"),
        (s.remote.is_empty(), s.remote_missing, had_r, "Remote-Ordner"),
    ] {
        if empty && had > 0 {
            let what = if missing { "existiert nicht" } else { "ist leer" };
            let msg = format!(
                "{side} {what}, beim letzten Sync waren dort {had} Dateien. Abbruch zum Schutz \
                 (nicht eingehängt? falscher Pfad?). Wenn das Absicht ist: Sync-Status zurücksetzen."
            );
            if mode == Mode::Diff {
                warnings.push(msg);
            } else {
                bail!("[{}] {msg}", pair.label());
            }
        }
    }

    let mut items = Vec::new();
    let mut in_sync = Vec::new();
    let all: BTreeSet<&String> = s.local.keys().chain(s.remote.keys()).chain(st.files.keys()).collect();
    let item = |path: &str, action, reason: &str| Item {
        path: path.to_string(),
        action,
        reason: reason.to_string(),
        local: s.local.get(path).copied(),
        remote: s.remote.get(path).copied(),
        selected: true,
    };

    if mode != Mode::TwoWay {
        let mut both = Vec::new();
        for p in all.iter().filter(|p| s.local.contains_key(**p) || s.remote.contains_key(**p)) {
            let p = p.as_str();
            match (s.local.contains_key(p), s.remote.contains_key(p), mode) {
                (true, true, _) => both.push(p.to_string()),
                (false, true, Mode::Diff) => items.push(item(p, Action::OnlyRemote, "")),
                (true, false, Mode::Diff) => items.push(item(p, Action::OnlyLocal, "")),
                (false, true, Mode::Pull) => items.push(item(p, Action::CopyToLocal, "nur remote vorhanden")),
                (true, false, Mode::Pull) => items.push(item(p, Action::DeleteLocal, "nur lokal vorhanden")),
                (true, false, _) => items.push(item(p, Action::CopyToRemote, "nur lokal vorhanden")),
                (false, true, _) => items.push(item(p, Action::DeleteRemote, "nur remote vorhanden")),
                (false, false, _) => {}
            }
        }
        let same = identical(&rc, cfg, pair, &s, &st, &both)?;
        for p in both {
            if same.contains(&p) {
                in_sync.push(p);
                continue;
            }
            let (l, r) = (s.local[&p], s.remote[&p]);
            let newer = if l.mtime_ms > r.mtime_ms { "lokal neuer" } else { "remote neuer" };
            let action = match mode {
                Mode::Diff => Action::Different,
                Mode::Pull => Action::OverwriteLocal,
                _ => Action::OverwriteRemote,
            };
            items.push(item(&p, action, &format!("Inhalt unterschiedlich, {newer}")));
        }
    } else {
        let mut both_changed = Vec::new();
        for p in all {
            let prev = st.files.get(p);
            let lc = change(s.local.get(p), prev.and_then(|e| e.l));
            let rc_ = change(s.remote.get(p), prev.and_then(|e| e.r));
            use Ch::*;
            let lr = |c: Ch, side: &str| match c {
                New => format!("{side} neu"),
                Modified => format!("{side} geändert"),
                Deleted => format!("{side} gelöscht"),
                _ => String::new(),
            };
            match (lc, rc_) {
                (Absent, Absent) => {}
                (Unchanged, Unchanged) => in_sync.push(p.clone()),
                (Deleted, Deleted) | (Deleted, Absent) | (Absent, Deleted) => in_sync.push(p.clone()),
                (Unchanged, Deleted) => items.push(item(p, Action::DeleteLocal, "remote gelöscht")),
                (Deleted, Unchanged) => items.push(item(p, Action::DeleteRemote, "lokal gelöscht")),
                (New | Modified, Unchanged) => items.push(item(p, Action::OverwriteRemote, &lr(lc, "lokal"))),
                (Unchanged, New | Modified) => items.push(item(p, Action::OverwriteLocal, &lr(rc_, "remote"))),
                (New | Modified | Unchanged, Absent) => items.push(item(p, Action::CopyToRemote, "nur lokal vorhanden")),
                (Absent, New | Modified | Unchanged) => items.push(item(p, Action::CopyToLocal, "nur remote vorhanden")),
                (New | Modified, Deleted) => {
                    items.push(item(p, Action::CopyToRemote, &format!("{}, remote gelöscht: wiederherstellen", lr(lc, "lokal"))))
                }
                (Deleted, New | Modified) => {
                    items.push(item(p, Action::CopyToLocal, &format!("{}, lokal gelöscht: wiederherstellen", lr(rc_, "remote"))))
                }
                (New | Modified, New | Modified) => both_changed.push((p.clone(), lc, rc_)),
            }
        }
        let paths: Vec<String> = both_changed.iter().map(|b| b.0.clone()).collect();
        let same = identical(&rc, cfg, pair, &s, &st, &paths)?;
        for (p, lc, rc_) in both_changed {
            if same.contains(&p) {
                in_sync.push(p);
            } else {
                let what = |c| if c == Ch::New { "neu" } else { "geändert" };
                items.push(item(&p, Action::Conflict, &format!("lokal {} und remote {}", what(lc), what(rc_))));
            }
        }
    }

    // Mass deletion guard: do not preselect deletes if they exceed the configured share.
    let deletes = items.iter().filter(|i| matches!(i.action, Action::DeleteLocal | Action::DeleteRemote)).count();
    let total = s.local.len().max(s.remote.len()).max(1);
    if deletes > 0 && deletes * 100 > total * cfg.max_delete_percent as usize {
        warnings.push(format!(
            "{deletes} von {total} Dateien ({} %) würden gelöscht, mehr als {} %. Die Löschungen sind \
             deshalb nicht vorausgewählt: bitte genau prüfen.",
            deletes * 100 / total,
            cfg.max_delete_percent
        ));
        for i in items.iter_mut().filter(|i| matches!(i.action, Action::DeleteLocal | Action::DeleteRemote)) {
            i.selected = false;
        }
    }

    items.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Plan {
        pair: pair.clone(),
        mode,
        items,
        in_sync,
        local_files: s.local.len(),
        remote_files: s.remote.len(),
        warnings,
    })
}

// ---------------------------------------------------------------- execution

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub done: usize,
    pub failed: Vec<String>,
    /// (original path, local conflict file, remote conflict file)
    pub conflicts: Vec<(String, String, String)>,
}

impl Report {
    pub fn summary(&self) -> String {
        let mut s = format!("{} Aktionen ausgeführt", self.done);
        if !self.conflicts.is_empty() {
            s += &format!(", {} Konflikte (Konfliktdateien angelegt)", self.conflicts.len());
        }
        if !self.failed.is_empty() {
            s += &format!(", {} fehlgeschlagen", self.failed.len());
        }
        s
    }
}

/// Executes the selected items of `plan`. Re-plans first and aborts if the selected items no
/// longer match (files changed since the preview).
pub fn execute(cfg: &Config, planned: &Plan, progress: &mut dyn FnMut(&str)) -> Result<Report> {
    if planned.mode == Mode::Diff {
        bail!("Diff führt keine Aktionen aus");
    }
    let _lock = lock(10)?;
    let pair = &planned.pair;
    let label = pair.label();
    let selected: Vec<&Item> = planned.items.iter().filter(|i| i.selected && i.action.executable()).collect();

    progress("Prüfe, ob sich seit der Vorschau etwas geändert hat …");
    let fresh = plan(cfg, pair, planned.mode)?;
    let fresh_set: HashSet<(&str, Action)> = fresh.items.iter().map(|i| (i.path.as_str(), i.action)).collect();
    let stale: Vec<&str> =
        selected.iter().filter(|i| !fresh_set.contains(&(i.path.as_str(), i.action))).map(|i| i.path.as_str()).collect();
    if !stale.is_empty() {
        bail!(
            "[{label}] Seit der Vorschau haben sich Dateien geändert ({}). Nichts ausgeführt, bitte neu prüfen.",
            stale.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
        );
    }

    let rc = Rclone::new(cfg)?;
    let ts = timestamp();
    let lroot = local_root(pair);
    let mut report = Report::default();
    let mut touched: Vec<String> = Vec::new();
    let mut to_remote: Vec<String> = Vec::new();
    let mut to_local: Vec<String> = Vec::new();
    let backup = pair.remote_backup.trim();
    log::info(format!("[{label}] {} gestartet: {} Aktionen", planned.mode.label(), selected.len()));

    let fail = |report: &mut Report, path: &str, msg: String| {
        log::error(format!("[{label}] {path}: {msg}"));
        report.failed.push(format!("{path}: {msg}"));
    };

    for it in &selected {
        let p = it.path.as_str();
        match it.action {
            Action::CopyToRemote => {
                to_remote.push(p.into());
                touched.push(p.into());
            }
            Action::CopyToLocal => {
                to_local.push(p.into());
                touched.push(p.into());
            }
            Action::OverwriteRemote => {
                log::info(format!("[{label}] remote überschreiben: {p}"));
                to_remote.push(p.into());
                touched.push(p.into());
            }
            Action::OverwriteLocal => {
                // Old local version goes to the trash first; if that fails, do not overwrite.
                match trash::delete(local_path(pair, p)) {
                    Ok(()) => {
                        log::info(format!("[{label}] lokal in Papierkorb (wird ersetzt): {p}"));
                        to_local.push(p.into());
                        touched.push(p.into());
                    }
                    Err(e) => fail(&mut report, p, format!("Papierkorb fehlgeschlagen, nicht überschrieben: {e}")),
                }
            }
            Action::DeleteLocal => match trash::delete(local_path(pair, p)) {
                Ok(()) => {
                    log::info(format!("[{label}] lokal in Papierkorb: {p}"));
                    report.done += 1;
                    touched.push(p.into());
                }
                Err(e) => fail(&mut report, p, format!("Papierkorb fehlgeschlagen: {e}")),
            },
            Action::DeleteRemote => {
                progress(&format!("lösche remote {p}"));
                let args: Vec<String> = if backup.is_empty() {
                    vec!["deletefile".into(), join(&pair.remote, p)]
                } else {
                    vec!["moveto".into(), join(&pair.remote, p), join(backup, &format!("{ts}/{p}"))]
                };
                match rc.run_logged(&args, &mut |_| {}) {
                    Ok(true) => {
                        log::info(format!("[{label}] remote gelöscht: {p}"));
                        report.done += 1;
                        touched.push(p.into());
                    }
                    Ok(false) => fail(&mut report, p, "remote löschen fehlgeschlagen".into()),
                    Err(e) => fail(&mut report, p, e.to_string()),
                }
            }
            Action::Conflict => {
                let exists = |n: &str| local_path(pair, n).exists() || fresh_has_remote(&fresh, n);
                let mut ln = conflict_name(p, &ts, "local");
                let mut rn = conflict_name(p, &ts, "remote");
                let mut k = 2;
                while exists(&ln) || exists(&rn) {
                    ln = conflict_name(p, &format!("{ts}-{k}"), "local");
                    rn = conflict_name(p, &format!("{ts}-{k}"), "remote");
                    k += 1;
                }
                if let Err(e) = std::fs::rename(local_path(pair, p), local_path(pair, &ln)) {
                    fail(&mut report, p, format!("Konflikt: lokal umbenennen fehlgeschlagen: {e}"));
                    continue;
                }
                let args = vec!["moveto".into(), join(&pair.remote, p), join(&pair.remote, &rn)];
                match rc.run_logged(&args, &mut |_| {}) {
                    Ok(true) => {
                        log::warn(format!("[{label}] Konflikt {p} -> {ln} + {rn}"));
                        to_remote.push(ln.clone());
                        to_local.push(rn.clone());
                        touched.extend([p.to_string(), ln.clone(), rn.clone()]);
                        report.conflicts.push((p.to_string(), ln, rn));
                    }
                    _ => {
                        let _ = std::fs::rename(local_path(pair, &ln), local_path(pair, p));
                        fail(&mut report, p, "Konflikt: remote umbenennen fehlgeschlagen".into());
                    }
                }
            }
            _ => {}
        }
    }

    let bdir = (!backup.is_empty()).then(|| join(backup, &ts));
    progress(&format!("lade {} Dateien hoch …", to_remote.len()));
    let up = copy_batch(&rc, &to_remote, &lroot, &pair.remote, bdir, progress);
    progress(&format!("lade {} Dateien herunter …", to_local.len()));
    let down = copy_batch(&rc, &to_local, &pair.remote, &lroot, None, progress);
    for (r, what) in [(up, "Hochladen"), (down, "Herunterladen")] {
        match r {
            Ok(true) => {}
            Ok(false) => log::error(format!("[{label}] {what}: rclone meldete Fehler (Details oben im Log)")),
            Err(e) => log::error(format!("[{label}] {what}: {e}")),
        }
    }

    // Record the new state for everything that is consistent now; skipped/failed paths keep
    // their old state so they show up again next time.
    progress("aktualisiere Sync-Status …");
    let after = list_sides(&rc, pair)?;
    let mut st = load_state(pair);
    let mut copied = HashSet::new();
    copied.extend(to_remote.iter().cloned());
    copied.extend(to_local.iter().cloned());
    for p in fresh.in_sync.iter().chain(touched.iter()) {
        let (l, r) = (after.local.get(p).copied(), after.remote.get(p).copied());
        let consistent = match (l, r) {
            (None, None) => true,
            (Some(a), Some(b)) => a.size == b.size,
            _ => false,
        };
        if consistent {
            if l.is_none() {
                st.files.remove(p);
            } else {
                st.files.insert(p.clone(), StateEntry { l, r });
            }
            if copied.contains(p) && !report.conflicts.iter().any(|c| &c.1 == p || &c.2 == p) {
                report.done += 1;
            }
        } else if touched.contains(p) && !report.failed.iter().any(|f| f.starts_with(&format!("{p}:"))) {
            report.failed.push(format!("{p}: nach dem Sync nicht auf beiden Seiten gleich"));
        }
    }
    report.done += report.conflicts.len();
    st.last_sync = Some(chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string());
    st.last_result = format!("{}: {}", planned.mode.label(), report.summary());
    save_state(pair, &st).context("Sync-Status speichern")?;
    log::info(format!("[{label}] fertig: {}", report.summary()));
    for f in &report.failed {
        log::error(format!("[{label}] fehlgeschlagen: {f}"));
    }
    Ok(report)
}

/// Copies the listed relative paths from `src` to `dst` in one rclone run.
fn copy_batch(
    rc: &Rclone,
    list: &[String],
    src: &str,
    dst: &str,
    backup_dir: Option<String>,
    progress: &mut dyn FnMut(&str),
) -> Result<bool> {
    if list.is_empty() {
        return Ok(true);
    }
    let f = crate::rclone::tmp_file("files", &list.join("\n"))?;
    let mut args: Vec<String> = [
        "copy", src, dst, "--files-from-raw", &f.display().to_string(), "--ignore-times", "--no-traverse",
        "-v", "--stats", "2s", "--stats-one-line",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(b) = backup_dir {
        args.push("--backup-dir".into());
        args.push(b);
    }
    let ok = rc.run_logged(&args, progress);
    let _ = std::fs::remove_file(f);
    ok
}

fn fresh_has_remote(plan: &Plan, path: &str) -> bool {
    plan.items.iter().any(|i| i.path == path && i.remote.is_some()) || plan.in_sync.iter().any(|p| p == path)
}

/// Unattended two-way sync: executes only non-destructive actions (new files, conflicts).
/// Returns (report, number of deletes/overwrites waiting for confirmation).
pub fn auto_sync(cfg: &Config, pair: &Pair) -> Result<(Report, usize)> {
    let mut p = plan(cfg, pair, Mode::TwoWay)?;
    let mut pending = 0;
    for i in p.items.iter_mut() {
        if i.action.destructive() {
            i.selected = false;
            pending += 1;
        }
    }
    if p.items.iter().all(|i| !i.selected) {
        return Ok((Report::default(), pending));
    }
    let r = execute(cfg, &p, &mut |_| {})?;
    Ok((r, pending))
}

pub fn last_status(pair: &Pair) -> (Option<String>, String) {
    let st = load_state(pair);
    (st.last_sync, st.last_result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conflict_names() {
        let ts = "2026-11-21T10-15-30Z";
        assert_eq!(conflict_name("a/rechnung.pdf", ts, "local"), "a/rechnung-2026-11-21T10-15-30Z-local.pdf");
        assert_eq!(conflict_name("Makefile", ts, "remote"), "Makefile-2026-11-21T10-15-30Z-remote");
        assert_eq!(conflict_name("x/.bashrc", ts, "local"), "x/.bashrc-2026-11-21T10-15-30Z-local");
        assert_eq!(conflict_name("a.tar.gz", ts, "local"), "a.tar-2026-11-21T10-15-30Z-local.gz");
    }

    /// End-to-end test with the real rclone binary; the "remote" is a second local folder.
    /// Run with: cargo test -- --ignored --test-threads=1
    #[test]
    #[ignore]
    fn end_to_end() {
        use std::fs;
        // Portable mode for the test binary, so no user data is touched.
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        let _ = fs::create_dir_all(exe_dir.join(crate::config::PORTABLE_DIR));
        assert!(dirs().portable);

        let base = std::env::temp_dir().join(format!("draisine-e2e-{}", std::process::id()));
        let (l, r) = (base.join("local"), base.join("remote"));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(l.join("sub")).unwrap();
        fs::create_dir_all(&r).unwrap();
        let w = |p: &Path, s: &str| fs::write(p, s).unwrap();
        w(&l.join("a.txt"), "a");
        w(&l.join("b.txt"), "bb-local");
        w(&l.join("sub/c.txt"), "c");
        w(&r.join("b.txt"), "bb-remot"); // same size, (nearly) same mtime, different content
        w(&l.join("same.txt"), "same");
        w(&r.join("same.txt"), "same");
        w(&r.join("d.txt"), "d");

        let cfg = Config::default();
        let pair = Pair {
            name: "e2e".into(),
            local: l.display().to_string(),
            remote: r.display().to_string(),
            ..Default::default()
        };
        let acts = |p: &Plan| -> Vec<(String, Action)> { p.items.iter().map(|i| (i.path.clone(), i.action)).collect() };
        let has = |root: &Path, rel: &str| root.join(rel).exists();

        // 1. Diff
        let d = plan(&cfg, &pair, Mode::Diff).unwrap();
        assert_eq!(
            acts(&d),
            vec![
                ("a.txt".into(), Action::OnlyLocal),
                ("b.txt".into(), Action::Different),
                ("d.txt".into(), Action::OnlyRemote),
                ("sub/c.txt".into(), Action::OnlyLocal)
            ]
        );

        // 2. First two-way sync: copies + conflict, nothing deleted.
        let p = plan(&cfg, &pair, Mode::TwoWay).unwrap();
        assert_eq!(
            acts(&p),
            vec![
                ("a.txt".into(), Action::CopyToRemote),
                ("b.txt".into(), Action::Conflict),
                ("d.txt".into(), Action::CopyToLocal),
                ("sub/c.txt".into(), Action::CopyToRemote)
            ]
        );
        let rep = execute(&cfg, &p, &mut |_| {}).unwrap();
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert_eq!(rep.conflicts.len(), 1);
        let (_, ln, rn) = rep.conflicts[0].clone();
        assert!(ln.starts_with("b-") && ln.ends_with("Z-local.txt"), "{ln}");
        assert!(rn.ends_with("Z-remote.txt"), "{rn}");
        for root in [&l, &r] {
            assert!(!has(root, "b.txt"));
            assert_eq!(fs::read_to_string(root.join(&ln)).unwrap(), "bb-local");
            assert_eq!(fs::read_to_string(root.join(&rn)).unwrap(), "bb-remot");
            for f in ["a.txt", "d.txt", "sub/c.txt"] {
                assert!(has(root, f), "{f} missing in {}", root.display());
            }
        }
        assert!(plan(&cfg, &pair, Mode::TwoWay).unwrap().items.is_empty(), "in sync after first run");

        // 3. Local edit + remote delete: both need confirmation; auto-sync must not do them.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        w(&l.join("a.txt"), "a2-changed");
        fs::remove_file(r.join("d.txt")).unwrap();
        w(&r.join("e.txt"), "new remote");
        let p = plan(&cfg, &pair, Mode::TwoWay).unwrap();
        assert_eq!(
            acts(&p),
            vec![
                ("a.txt".into(), Action::OverwriteRemote),
                ("d.txt".into(), Action::DeleteLocal),
                ("e.txt".into(), Action::CopyToLocal)
            ]
        );
        let (rep, pending) = auto_sync(&cfg, &pair).unwrap();
        assert_eq!(pending, 2);
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert!(has(&l, "e.txt"), "auto-sync copies new files");
        assert_eq!(fs::read_to_string(r.join("a.txt")).unwrap(), "a", "auto-sync must not overwrite");
        assert!(has(&l, "d.txt"), "auto-sync must not delete");
        // Declined actions are still pending afterwards.
        let p = plan(&cfg, &pair, Mode::TwoWay).unwrap();
        assert_eq!(acts(&p), vec![("a.txt".into(), Action::OverwriteRemote), ("d.txt".into(), Action::DeleteLocal)]);

        // 4. Confirmed execution. Content changes that keep the same actions are fine…
        w(&l.join("a.txt"), "a3-changed-again");
        assert!(execute(&cfg, &p, &mut |_| {}).is_ok());
        assert_eq!(fs::read_to_string(r.join("a.txt")).unwrap(), "a3-changed-again");
        assert!(!has(&l, "d.txt"), "deleted (moved to trash)");
        // …but a plan whose actions no longer match is refused.
        w(&l.join("x.txt"), "x");
        let p = plan(&cfg, &pair, Mode::TwoWay).unwrap();
        fs::remove_file(l.join("x.txt")).unwrap();
        assert!(execute(&cfg, &p, &mut |_| {}).is_err(), "stale plan must be refused");

        // 5. Pull mirrors remote -> local, asks before deleting the local-only file.
        w(&l.join("only-local.txt"), "ol");
        let p = plan(&cfg, &pair, Mode::Pull).unwrap();
        assert_eq!(acts(&p), vec![("only-local.txt".into(), Action::DeleteLocal)]);

        // 6. Safety: emptied remote is never treated as "delete everything".
        let p = plan(&cfg, &pair, Mode::TwoWay).unwrap();
        execute(&cfg, &p, &mut |_| {}).unwrap();
        fs::remove_dir_all(&r).unwrap();
        fs::create_dir_all(&r).unwrap();
        let e = plan(&cfg, &pair, Mode::TwoWay).unwrap_err().to_string();
        assert!(e.contains("Remote-Ordner ist leer"), "{e}");

        reset_state(&pair);
        let _ = fs::remove_dir_all(&base);
    }
}
