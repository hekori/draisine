//! Sync engine. rclone is only used as a transport (list / check / copy / delete); the decision
//! what to do with each file is made here, so the user can confirm every delete and overwrite.
//!
//! Every action is planned first ([`plan`]), shown to the user, and only then executed
//! ([`execute`]). Before executing, the plan is computed again and compared: if anything changed
//! in the meantime the execution is aborted.
//!
//! Two-way sync compares each side with its own listing from the last successful sync
//! (stored in the state file), so it knows which side changed. A file renamed on one side
//! (old name deleted, new name with the same content created) is renamed on the other side too.
//! Files changed on both sides are conflicts: the cloud version keeps the name, the local version
//! is kept next to it as `name (<computer>'s conflicted copy YYYY-MM-DD).ext` on both sides.

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
            Mode::Diff => "Diff local ↔ remote",
            Mode::Pull => "One-way sync remote ➡ local",
            Mode::Push => "One-way sync local ➡ remote",
            Mode::TwoWay => "Two-way sync local ↔ remote",
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
    /// Renamed in the cloud: rename the local file `from` -> `path` too.
    RenameLocal,
    /// Renamed locally: rename the remote file `from` -> `path` too.
    RenameRemote,
    /// Changed on both sides: the cloud version keeps the name, the local version is kept as
    /// `copy_name` on both sides.
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
            Action::CopyToRemote => "➡ upload",
            Action::CopyToLocal => "⬅ download",
            Action::OverwriteRemote => "➡ overwrite remote",
            Action::OverwriteLocal => "⬅ overwrite local",
            Action::DeleteRemote => "✖ delete remote",
            Action::DeleteLocal => "✖ delete local (trash)",
            Action::RenameLocal => "⬅ rename local",
            Action::RenameRemote => "➡ rename remote",
            Action::Conflict => "⚠ conflict: keep both",
            Action::OnlyLocal => "local only",
            Action::OnlyRemote => "remote only",
            Action::Different => "different",
        }
    }
}

/// What happened to a file on one side since the last sync.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    /// Not there now and not at the last sync.
    Absent,
    New,
    Unchanged,
    Modified,
    Deleted,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub path: String,
    pub action: Action,
    pub reason: String,
    pub local: Option<Entry>,
    pub remote: Option<Entry>,
    pub local_change: Change,
    pub remote_change: Change,
    /// Renames: the old path (`path` is the new one). `local`/`remote` describe the file under
    /// its current name on each side, i.e. the old one on the side that is renamed.
    pub from: Option<String>,
    /// Conflicts: the name the local version gets (on both sides).
    pub copy_name: Option<String>,
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
    /// No sync state yet (first two-way sync): "new" means "not on the other side".
    pub first_sync: bool,
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
    /// Deletes and overwrites the last two-way run found but did not apply.
    #[serde(default)]
    pub pending: Pending,
}

/// Deletes and overwrites waiting for the user's confirmation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub deletes: usize,
    pub overwrites: usize,
}

impl Pending {
    fn of<'a>(items: impl Iterator<Item = &'a Item>) -> Pending {
        let mut p = Pending::default();
        for i in items {
            match i.action {
                Action::DeleteLocal | Action::DeleteRemote => p.deletes += 1,
                Action::OverwriteLocal | Action::OverwriteRemote => p.overwrites += 1,
                _ => {}
            }
        }
        p
    }

    pub fn total(&self) -> usize {
        self.deletes + self.overwrites
    }

    /// E.g. "1 deletion, 2 overwrites".
    pub fn describe(&self) -> String {
        let n = |k: usize, one: &str, many: &str| format!("{k} {}", if k == 1 { one } else { many });
        let mut parts = Vec::new();
        if self.deletes > 0 {
            parts.push(n(self.deletes, "deletion", "deletions"));
        }
        if self.overwrites > 0 {
            parts.push(n(self.overwrites, "overwrite", "overwrites"));
        }
        parts.join(", ")
    }
}

/// "1 change", "3 changes".
pub fn changes(n: usize) -> String {
    format!("{n} change{}", if n == 1 { "" } else { "s" })
}

/// The deletes and overwrites among `items` that wait for the user: all of them, except those
/// auto-sync will apply on its own (see [`auto_safe`]) when it runs for this pair.
fn waiting<'a>(cfg: &Config, pair: &Pair, st: &State, items: impl Iterator<Item = &'a Item>) -> Pending {
    let auto = pair.auto_sync && !cfg.auto_sync_paused && st.last_sync.is_some();
    Pending::of(items.filter(|i| !(auto && auto_safe(i, st))))
}

/// Remembers what a two-way plan would delete or overwrite, so the overview and the tray can
/// point the user to it. Other modes say nothing about two-way and leave it alone.
pub fn record_pending(cfg: &Config, plan: &Plan) {
    if plan.mode != Mode::TwoWay {
        return;
    }
    let mut st = load_state(&plan.pair);
    let p = waiting(cfg, &plan.pair, &st, plan.items.iter());
    if st.pending != p {
        st.pending = p;
        if let Err(e) = save_state(&plan.pair, &st) {
            log::warn(format!("[{}] Saving the sync state failed: {e:#}", plan.pair.label()));
        }
    }
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
    log::info(format!("[{}] Sync state reset", pair.label()));
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
                    bail!("Another sync is running. Please try again later.");
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

/// `dir/name.ext` -> `dir/name (<who>'s conflicted copy <date>[ n]).ext` (like Dropbox).
/// `n` > 1 is appended when the plain name is taken.
pub fn conflict_name(path: &str, who: &str, date: &str, n: usize) -> String {
    let (dir, file) = match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    };
    let (stem, ext) = match file.rfind('.') {
        Some(i) if i > 0 && i + 1 < file.len() => (&file[..i], &file[i..]),
        _ => (file, ""),
    };
    let num = if n > 1 { format!(" {n}") } else { String::new() };
    format!("{dir}{stem} ({who}'s conflicted copy {date}{num}){ext}")
}

/// Name of this computer for conflict copies: the host name without domain, without characters
/// that are not allowed in file names.
pub fn device_name() -> String {
    let h = gethostname::gethostname().to_string_lossy().to_string();
    let h = h.split('.').next().unwrap_or("").trim();
    let h: String = h.chars().filter(|c| !r#"\/:*?"<>|'"#.contains(*c) && !c.is_control()).collect();
    if h.is_empty() { "this computer".into() } else { h }
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

fn change(cur: Option<&Entry>, prev: Option<Entry>) -> Change {
    match (cur, prev) {
        (None, None) => Change::Absent,
        (Some(_), None) => Change::New,
        (None, Some(_)) => Change::Deleted,
        (Some(a), Some(b)) if *a == b => Change::Unchanged,
        _ => Change::Modified,
    }
}

// ---------------------------------------------------------------- planning

pub fn plan(cfg: &Config, pair: &Pair, mode: Mode) -> Result<Plan> {
    plan_with(cfg, pair, mode, None)
}

/// Like [`plan`], but stops (killing the running rclone) as soon as `cancel` is set.
pub fn plan_with(cfg: &Config, pair: &Pair, mode: Mode, cancel: Option<Cancel>) -> Result<Plan> {
    if pair.local.trim().is_empty() || pair.remote.trim().is_empty() {
        bail!("[{}] local folder and remote must both be set", pair.label());
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
        (s.local.is_empty(), s.local_missing, had_l, "Local folder"),
        (s.remote.is_empty(), s.remote_missing, had_r, "Remote folder"),
    ] {
        if empty && had > 0 {
            let what = if missing { "does not exist" } else { "is empty" };
            let msg = format!(
                "{side} {what}, but it had {had} files at the last sync. Stopped as a precaution \
                 (not mounted? wrong path?). If this is intended: reset the sync state."
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
    let item = |path: &str, action, reason: &str| {
        let prev = st.files.get(path);
        Item {
            path: path.to_string(),
            action,
            reason: reason.to_string(),
            local: s.local.get(path).copied(),
            remote: s.remote.get(path).copied(),
            local_change: change(s.local.get(path), prev.and_then(|e| e.l)),
            remote_change: change(s.remote.get(path), prev.and_then(|e| e.r)),
            from: None,
            copy_name: None,
            selected: true,
        }
    };

    if mode != Mode::TwoWay {
        let mut both = Vec::new();
        for p in all.iter().filter(|p| s.local.contains_key(**p) || s.remote.contains_key(**p)) {
            let p = p.as_str();
            match (s.local.contains_key(p), s.remote.contains_key(p), mode) {
                (true, true, _) => both.push(p.to_string()),
                (false, true, Mode::Diff) => items.push(item(p, Action::OnlyRemote, "")),
                (true, false, Mode::Diff) => items.push(item(p, Action::OnlyLocal, "")),
                (false, true, Mode::Pull) => items.push(item(p, Action::CopyToLocal, "exists only on remote")),
                (true, false, Mode::Pull) => items.push(item(p, Action::DeleteLocal, "exists only locally")),
                (true, false, _) => items.push(item(p, Action::CopyToRemote, "exists only locally")),
                (false, true, _) => items.push(item(p, Action::DeleteRemote, "exists only on remote")),
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
            let newer = if l.mtime_ms > r.mtime_ms { "local newer" } else { "remote newer" };
            let action = match mode {
                Mode::Diff => Action::Different,
                Mode::Pull => Action::OverwriteLocal,
                _ => Action::OverwriteRemote,
            };
            items.push(item(&p, action, &format!("content differs, {newer}")));
        }
    } else {
        let mut both_changed = Vec::new();
        for p in all {
            let prev = st.files.get(p);
            let lc = change(s.local.get(p), prev.and_then(|e| e.l));
            let rc_ = change(s.remote.get(p), prev.and_then(|e| e.r));
            use Change::*;
            let lr = |c: Change, side: &str| match c {
                New => format!("{side} new"),
                Modified => format!("{side} changed"),
                Deleted => format!("{side} deleted"),
                _ => String::new(),
            };
            match (lc, rc_) {
                (Absent, Absent) => {}
                (Unchanged, Unchanged) => in_sync.push(p.clone()),
                (Deleted, Deleted) | (Deleted, Absent) | (Absent, Deleted) => in_sync.push(p.clone()),
                (Unchanged, Deleted) => items.push(item(p, Action::DeleteLocal, "deleted on remote")),
                (Deleted, Unchanged) => items.push(item(p, Action::DeleteRemote, "deleted locally")),
                (New | Modified, Unchanged) => items.push(item(p, Action::OverwriteRemote, &lr(lc, "local"))),
                (Unchanged, New | Modified) => items.push(item(p, Action::OverwriteLocal, &lr(rc_, "remote"))),
                (New | Modified | Unchanged, Absent) => items.push(item(p, Action::CopyToRemote, "exists only locally")),
                (Absent, New | Modified | Unchanged) => items.push(item(p, Action::CopyToLocal, "exists only on remote")),
                (New | Modified, Deleted) => {
                    items.push(item(p, Action::CopyToRemote, &format!("{}, deleted on remote: restore", lr(lc, "local"))))
                }
                (Deleted, New | Modified) => {
                    items.push(item(p, Action::CopyToLocal, &format!("{}, deleted locally: restore", lr(rc_, "remote"))))
                }
                (New | Modified, New | Modified) => both_changed.push((p.clone(), lc, rc_)),
            }
        }
        let paths: Vec<String> = both_changed.iter().map(|b| b.0.clone()).collect();
        let same = identical(&rc, cfg, pair, &s, &st, &paths)?;
        let (who, date) = (device_name(), chrono::Local::now().format("%Y-%m-%d").to_string());
        let mut taken: HashSet<String> = s.local.keys().chain(s.remote.keys()).cloned().collect();
        for (p, lc, rc_) in both_changed {
            if same.contains(&p) {
                in_sync.push(p);
            } else {
                let what = |c| if c == Change::New { "new" } else { "changed" };
                let mut it = item(&p, Action::Conflict, &format!("local {} and remote {}", what(lc), what(rc_)));
                let copy = (1..).map(|n| conflict_name(&p, &who, &date, n)).find(|n| !taken.contains(n)).unwrap();
                taken.insert(copy.clone());
                it.copy_name = Some(copy);
                items.push(it);
            }
        }
        if let Err(e) = detect_renames(&rc, pair, &mut items) {
            log::warn(format!("[{}] Rename detection skipped: {e:#}", pair.label()));
        }
    }

    // Mass deletion guard: do not preselect deletes if they exceed the configured share.
    let deletes = items.iter().filter(|i| matches!(i.action, Action::DeleteLocal | Action::DeleteRemote)).count();
    let total = s.local.len().max(s.remote.len()).max(1);
    if deletes > 0 && deletes * 100 > total * cfg.max_delete_percent as usize {
        warnings.push(format!(
            "{deletes} of {total} files ({} %) would be deleted, more than {} %. The deletions are \
             therefore not preselected: please check carefully.",
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
        first_sync: st.files.is_empty(),
        warnings,
    })
}

/// Turns "deleted on one side" + "new on the same side" with the same content into one rename,
/// which is then repeated on the other side instead of deleting there and copying the file again.
/// Only unambiguous cases: exactly one deleted and one new file with that size and content hash.
fn detect_renames(rc: &Rclone, pair: &Pair, items: &mut Vec<Item>) -> Result<()> {
    for in_cloud in [true, false] {
        let (del, new, rename) = if in_cloud {
            (Action::DeleteLocal, Action::CopyToLocal, Action::RenameLocal)
        } else {
            (Action::DeleteRemote, Action::CopyToRemote, Action::RenameRemote)
        };
        // The old name still exists on the unchanged side, the new one only on the changed side.
        let unchanged_side = |i: &Item| if in_cloud { i.local } else { i.remote };
        let changed_side = |i: &Item| if in_cloud { i.remote } else { i.local };
        let is_new = |i: &Item| {
            let (u, c) = if in_cloud { (i.local_change, i.remote_change) } else { (i.remote_change, i.local_change) };
            i.action == new && u == Change::Absent && c == Change::New
        };
        let mut by_size: BTreeMap<i64, (Vec<usize>, Vec<usize>)> = BTreeMap::new();
        for (k, i) in items.iter().enumerate() {
            if i.action == del && let Some(e) = unchanged_side(i) {
                by_size.entry(e.size).or_default().0.push(k);
            } else if is_new(i) && let Some(e) = changed_side(i) {
                by_size.entry(e.size).or_default().1.push(k);
            }
        }
        // Only sizes that occur among both the deleted and the new files can be renames.
        let (olds, news): (Vec<usize>, Vec<usize>) = by_size
            .into_values()
            .filter(|(o, n)| !o.is_empty() && !n.is_empty())
            .fold((Vec::new(), Vec::new()), |(mut a, mut b), (o, n)| {
                a.extend(o);
                b.extend(n);
                (a, b)
            });
        if olds.is_empty() || olds.len() + news.len() > 2000 {
            continue;
        }
        let old_paths: Vec<String> = olds.iter().map(|&k| items[k].path.clone()).collect();
        let new_paths: Vec<String> = news.iter().map(|&k| items[k].path.clone()).collect();
        let (remote_paths, local_paths) = if in_cloud { (&new_paths, &old_paths) } else { (&old_paths, &new_paths) };
        // The cloud decides the hash type (it can only report its own), the local side computes it.
        let rh = rc.hashes(&pair.remote, remote_paths, None)?;
        let ty = rh
            .values()
            .next()
            .and_then(|h| {
                ["md5", "sha1"].iter().map(|t| t.to_string()).find(|t| h.contains_key(t)).or_else(|| h.keys().min().cloned())
            })
            .context("the cloud reports no hash")?;
        let lh = rc.hashes(&local_root(pair), local_paths, Some(&ty))?;
        let (oh, nh) = if in_cloud { (&lh, &rh) } else { (&rh, &lh) };
        // Group by (size, hash); a rename needs exactly one deleted and one new file per group.
        let mut groups: BTreeMap<(i64, String), (Vec<usize>, Vec<usize>)> = BTreeMap::new();
        for (list, side_hashes, old) in [(&olds, oh, true), (&news, nh, false)] {
            for &k in list {
                let e = if old { unchanged_side(&items[k]) } else { changed_side(&items[k]) };
                if let (Some(e), Some(h)) = (e, side_hashes.get(&items[k].path).and_then(|h| h.get(&ty))) {
                    let g = groups.entry((e.size, h.clone())).or_default();
                    if old { g.0.push(k) } else { g.1.push(k) }
                }
            }
        }
        let mut remove = HashSet::new();
        for (o, n) in groups.into_values().filter(|(o, n)| o.len() == 1 && n.len() == 1).map(|(o, n)| (o[0], n[0])) {
            let op = items[o].path.clone();
            let old = items[o].clone();
            let it = &mut items[n];
            it.action = rename;
            it.reason = format!("renamed {}", if in_cloud { "in the cloud" } else { "on this computer" });
            if in_cloud {
                it.local = old.local;
            } else {
                it.remote = old.remote;
            }
            it.from = Some(op);
            remove.insert(o);
        }
        let mut k = 0;
        items.retain(|_| {
            k += 1;
            !remove.contains(&(k - 1))
        });
    }
    Ok(())
}

// ---------------------------------------------------------------- execution

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub done: usize,
    pub failed: Vec<String>,
    /// (path, conflicted copy): the path holds the cloud version, the copy the local version.
    pub conflicts: Vec<(String, String)>,
}

impl Report {
    pub fn summary(&self) -> String {
        let mut s = format!("{} actions applied", self.done);
        if !self.conflicts.is_empty() {
            s += &format!(", {} conflicts (conflicted copies created)", self.conflicts.len());
        }
        if !self.failed.is_empty() {
            s += &format!(", {} failed", self.failed.len());
        }
        s
    }
}

/// Executes the selected items of `plan`. Re-plans first and aborts if the selected items no
/// longer match (files changed since the preview).
pub fn execute(cfg: &Config, planned: &Plan, progress: &mut dyn FnMut(&str)) -> Result<Report> {
    if planned.mode == Mode::Diff {
        bail!("Diff does not apply any actions");
    }
    let _lock = lock(10)?;
    let pair = &planned.pair;
    let label = pair.label();
    let selected: Vec<&Item> = planned.items.iter().filter(|i| i.selected && i.action.executable()).collect();

    progress("Checking whether anything changed since the preview …");
    let fresh = plan(cfg, pair, planned.mode)?;
    let fresh_set: HashSet<(&str, Action)> = fresh.items.iter().map(|i| (i.path.as_str(), i.action)).collect();
    let stale: Vec<&str> =
        selected.iter().filter(|i| !fresh_set.contains(&(i.path.as_str(), i.action))).map(|i| i.path.as_str()).collect();
    if !stale.is_empty() {
        bail!(
            "[{label}] Files changed since the preview ({}). Nothing applied, please check again.",
            stale.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
        );
    }

    let rc = Rclone::new(cfg)?;
    let ts = timestamp();
    let mut selected = selected;
    // Renames first: a conflicted copy or download must not take a name that is renamed away.
    selected.sort_by_key(|i| !matches!(i.action, Action::RenameLocal | Action::RenameRemote));
    let lroot = local_root(pair);
    let mut report = Report::default();
    let mut touched: Vec<String> = Vec::new();
    let mut to_remote: Vec<String> = Vec::new();
    let mut to_local: Vec<String> = Vec::new();
    let backup = pair.remote_backup.trim();
    log::info(format!("[{label}] {} started: {} actions", planned.mode.label(), selected.len()));

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
                log::info(format!("[{label}] overwrite remote: {p}"));
                to_remote.push(p.into());
                touched.push(p.into());
            }
            Action::OverwriteLocal => {
                // Old local version goes to the trash first; if that fails, do not overwrite.
                match trash::delete(local_path(pair, p)) {
                    Ok(()) => {
                        log::info(format!("[{label}] local to trash (being replaced): {p}"));
                        to_local.push(p.into());
                        touched.push(p.into());
                    }
                    Err(e) => fail(&mut report, p, format!("Moving to trash failed, not overwritten: {e}")),
                }
            }
            Action::DeleteLocal => match trash::delete(local_path(pair, p)) {
                Ok(()) => {
                    log::info(format!("[{label}] local to trash: {p}"));
                    report.done += 1;
                    touched.push(p.into());
                }
                Err(e) => fail(&mut report, p, format!("Moving to trash failed: {e}")),
            },
            Action::DeleteRemote => {
                progress(&format!("deleting remote {p}"));
                let args: Vec<String> = if backup.is_empty() {
                    vec!["deletefile".into(), join(&pair.remote, p)]
                } else {
                    vec!["moveto".into(), join(&pair.remote, p), join(backup, &format!("{ts}/{p}"))]
                };
                match rc.run_logged(&args, &mut |_| {}) {
                    Ok(true) => {
                        log::info(format!("[{label}] deleted on remote: {p}"));
                        report.done += 1;
                        touched.push(p.into());
                    }
                    Ok(false) => fail(&mut report, p, "deleting on remote failed".into()),
                    Err(e) => fail(&mut report, p, e.to_string()),
                }
            }
            Action::RenameLocal | Action::RenameRemote => {
                let from = it.from.as_deref().unwrap_or_default();
                let local = it.action == Action::RenameLocal;
                let taken = if local { local_path(pair, p).exists() } else { fresh_has_remote(&fresh, p) };
                if taken {
                    fail(&mut report, p, format!("Renaming {from} failed: {p} already exists"));
                    continue;
                }
                let r = if local {
                    let dst = local_path(pair, p);
                    dst.parent()
                        .map_or(Ok(()), std::fs::create_dir_all)
                        .and_then(|_| std::fs::rename(local_path(pair, from), &dst))
                        .map_err(|e| e.to_string())
                } else {
                    let args = vec!["moveto".into(), join(&pair.remote, from), join(&pair.remote, p)];
                    match rc.run_logged(&args, &mut |_| {}) {
                        Ok(true) => Ok(()),
                        Ok(false) => Err("rclone moveto failed".into()),
                        Err(e) => Err(e.to_string()),
                    }
                };
                match r {
                    Ok(()) => {
                        log::info(format!("[{label}] renamed {}: {from} -> {p}", if local { "local" } else { "remote" }));
                        report.done += 1;
                        touched.extend([from.to_string(), p.to_string()]);
                    }
                    Err(e) => fail(&mut report, p, format!("Renaming {from} failed: {e}")),
                }
            }
            Action::Conflict => {
                // The cloud version keeps the name; the local version is kept as a conflicted
                // copy (uploaded too), then the cloud version is downloaded under the name.
                let Some(copy) = it.copy_name.clone() else { continue };
                if local_path(pair, &copy).exists() || fresh_has_remote(&fresh, &copy) {
                    fail(&mut report, p, format!("Conflict: {copy} already exists"));
                    continue;
                }
                if let Err(e) = std::fs::rename(local_path(pair, p), local_path(pair, &copy)) {
                    fail(&mut report, p, format!("Conflict: renaming the local version failed: {e}"));
                    continue;
                }
                log::warn(format!("[{label}] conflict {p}: local version kept as {copy}"));
                to_remote.push(copy.clone());
                to_local.push(p.into());
                touched.extend([p.to_string(), copy.clone()]);
                report.conflicts.push((p.to_string(), copy));
            }
            _ => {}
        }
    }

    let bdir = (!backup.is_empty()).then(|| join(backup, &ts));
    progress(&format!("uploading {} files …", to_remote.len()));
    let up = copy_batch(&rc, &to_remote, &lroot, &pair.remote, bdir, progress);
    progress(&format!("downloading {} files …", to_local.len()));
    let down = copy_batch(&rc, &to_local, &pair.remote, &lroot, None, progress);
    for (r, what) in [(up, "Upload"), (down, "Download")] {
        match r {
            Ok(true) => {}
            Ok(false) => log::error(format!("[{label}] {what}: rclone reported errors (details above in the log)")),
            Err(e) => log::error(format!("[{label}] {what}: {e}")),
        }
    }

    // Record the new state for everything that is consistent now; skipped/failed paths keep
    // their old state so they show up again next time.
    progress("updating sync state …");
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
            if copied.contains(p) && !report.conflicts.iter().any(|c| &c.0 == p || &c.1 == p) {
                report.done += 1;
            }
        } else if touched.contains(p) && !report.failed.iter().any(|f| f.starts_with(&format!("{p}:"))) {
            report.failed.push(format!("{p}: not identical on both sides after the sync"));
        }
    }
    report.done += report.conflicts.len();
    st.last_sync = Some(chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string());
    st.last_result = format!("{}: {}", planned.mode.label(), report.summary());
    // What is still waiting: two-way deletes/overwrites that were not selected. A one-way sync
    // changes the baseline, so the next two-way run will tell.
    let applied: HashSet<(&str, Action)> = selected.iter().map(|i| (i.path.as_str(), i.action)).collect();
    st.pending = if planned.mode == Mode::TwoWay {
        waiting(cfg, pair, &st, fresh.items.iter().filter(|i| !applied.contains(&(i.path.as_str(), i.action))))
    } else {
        Pending::default()
    };
    save_state(pair, &st).context("saving sync state")?;
    log::info(format!("[{label}] done: {}", report.summary()));
    for f in &report.failed {
        log::error(format!("[{label}] failed: {f}"));
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
    // A remote rename's `remote` entry describes the old name, not `path`.
    plan.items.iter().any(|i| i.path == path && i.remote.is_some() && i.action != Action::RenameRemote)
        || plan.in_sync.iter().any(|p| p == path)
}

/// The pair has a recorded sync for its current folders and excludes. Auto-sync requires this:
/// the first sync is always done by hand, so the user has seen what it does.
pub fn has_synced(pair: &Pair) -> bool {
    load_state(pair).last_sync.is_some()
}

/// The background auto-sync includes this pair.
pub fn auto_sync_active(pair: &Pair) -> bool {
    pair.auto_sync && has_synced(pair)
}

/// Whether auto-sync may apply this two-way item without asking. Non-destructive actions always
/// can. A delete or overwrite only when it is clear which side leads and the other side is exactly
/// as the sync history recorded it:
/// - the file was in sync at the last sync (recorded on both sides),
/// - the side that is replaced or deleted still has the recorded size and modification time
///   (nobody touched it since; otherwise it needs a look),
/// - the leading side deleted the file, or changed it with a modification time after the recorded
///   one (a file that went back in time, e.g. restored from a backup, needs a look),
/// - the mass deletion guard did not deselect it.
pub fn auto_safe(it: &Item, st: &State) -> bool {
    use Action::*;
    if !it.action.destructive() {
        return true;
    }
    let Some(&StateEntry { l: Some(sl), r: Some(sr) }) = st.files.get(&it.path) else { return false };
    let (src_rec, src_now, dst_rec, dst_now) = match it.action {
        OverwriteRemote | DeleteRemote => (sl, it.local, sr, it.remote),
        OverwriteLocal | DeleteLocal => (sr, it.remote, sl, it.local),
        _ => return false,
    };
    let leading_ok = match (it.action, src_now) {
        (DeleteRemote | DeleteLocal, None) => true,
        (OverwriteRemote | OverwriteLocal, Some(now)) => now.mtime_ms > src_rec.mtime_ms,
        _ => false,
    };
    it.selected && dst_now == Some(dst_rec) && leading_ok
}

/// Unattended two-way sync: executes new files, conflicts (keeping both versions) and the deletes
/// and overwrites that pass [`auto_safe`]. Returns (report, number of deletes/overwrites waiting
/// for confirmation).
pub fn auto_sync(cfg: &Config, pair: &Pair) -> Result<(Report, usize)> {
    if !has_synced(pair) {
        bail!("[{}] Auto-sync needs one manual sync first", pair.label());
    }
    let mut p = plan(cfg, pair, Mode::TwoWay)?;
    let st = load_state(pair);
    let mut pending = 0;
    for i in p.items.iter_mut() {
        if !auto_safe(i, &st) {
            i.selected = false;
            pending += 1;
        } else if i.action.destructive() {
            log::info(format!("[{}] auto-sync: {} {} ({})", pair.label(), i.action.label(), i.path, i.reason));
        }
    }
    if p.items.iter().all(|i| !i.selected) {
        record_pending(cfg, &p);
        return Ok((Report::default(), pending));
    }
    let r = execute(cfg, &p, &mut |_| {})?;
    Ok((r, pending))
}


#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conflict_names() {
        let (w, d) = ("laptop", "2026-11-22");
        assert_eq!(conflict_name("a/plan.txt", w, d, 1), "a/plan (laptop's conflicted copy 2026-11-22).txt");
        assert_eq!(conflict_name("plan.txt", w, d, 2), "plan (laptop's conflicted copy 2026-11-22 2).txt");
        assert_eq!(conflict_name("Makefile", w, d, 1), "Makefile (laptop's conflicted copy 2026-11-22)");
        assert_eq!(conflict_name("x/.bashrc", w, d, 1), "x/.bashrc (laptop's conflicted copy 2026-11-22)");
        assert_eq!(conflict_name("a.tar.gz", w, d, 1), "a.tar (laptop's conflicted copy 2026-11-22).gz");
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

        // 2. First two-way sync: copies + conflict, nothing deleted. Auto-sync is refused before it.
        assert!(!has_synced(&pair));
        assert!(auto_sync(&cfg, &pair).is_err(), "auto-sync needs one manual sync first");
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
        let copy = p.items.iter().find(|i| i.action == Action::Conflict).unwrap().copy_name.clone().unwrap();
        assert!(copy.starts_with("b (") && copy.contains("'s conflicted copy ") && copy.ends_with(").txt"), "{copy}");
        let rep = execute(&cfg, &p, &mut |_| {}).unwrap();
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert_eq!(rep.conflicts, vec![("b.txt".to_string(), copy.clone())]);
        for root in [&l, &r] {
            assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "bb-remot", "cloud version keeps the name");
            assert_eq!(fs::read_to_string(root.join(&copy)).unwrap(), "bb-local", "local version kept as copy");
            for f in ["a.txt", "d.txt", "sub/c.txt"] {
                assert!(has(root, f), "{f} missing in {}", root.display());
            }
        }
        assert!(plan(&cfg, &pair, Mode::TwoWay).unwrap().items.is_empty(), "in sync after first run");
        assert!(has_synced(&pair));

        // 3. Local edit + remote delete, the other side untouched since the last sync: it is clear
        // which side leads, so auto-sync applies both.
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
        let mut pair_auto = pair.clone();
        pair_auto.auto_sync = true;
        record_pending(&cfg, &p);
        assert_eq!(load_state(&pair).pending.total(), 2, "auto-sync off: both wait");
        record_pending(&cfg, &plan(&cfg, &pair_auto, Mode::TwoWay).unwrap());
        assert_eq!(load_state(&pair).pending.total(), 0, "auto-sync on: auto-sync will do them");
        let (rep, pending) = auto_sync(&cfg, &pair).unwrap();
        assert_eq!(pending, 0);
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert!(has(&l, "e.txt"), "auto-sync copies new files");
        assert_eq!(fs::read_to_string(r.join("a.txt")).unwrap(), "a2-changed", "auto-sync overwrites the remote");
        assert!(!has(&l, "d.txt"), "auto-sync deletes (moved to trash)");
        assert!(plan(&cfg, &pair, Mode::TwoWay).unwrap().items.is_empty(), "in sync after auto-sync");

        // 4. A change whose time does not fit the sync history (modification time before the
        // recorded one, e.g. restored from a backup) waits for confirmation.
        let rec = load_state(&pair).files["a.txt"].l.unwrap();
        w(&l.join("a.txt"), "a3-restored");
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_millis((rec.mtime_ms - 3_600_000) as u64);
        fs::File::options().write(true).open(l.join("a.txt")).unwrap().set_modified(old).unwrap();
        let (rep, pending) = auto_sync(&cfg, &pair).unwrap();
        assert_eq!((rep.done, pending), (0, 1));
        assert_eq!(fs::read_to_string(r.join("a.txt")).unwrap(), "a2-changed", "auto-sync must not overwrite");
        assert_eq!(load_state(&pair).pending, Pending { deletes: 0, overwrites: 1 }, "held back for the overview");
        let p = plan(&cfg, &pair, Mode::TwoWay).unwrap();
        assert_eq!(acts(&p), vec![("a.txt".into(), Action::OverwriteRemote)]);

        // Confirmed execution. Content changes that keep the same actions are fine…
        w(&l.join("a.txt"), "a4-changed-again");
        assert!(execute(&cfg, &p, &mut |_| {}).is_ok());
        assert_eq!(fs::read_to_string(r.join("a.txt")).unwrap(), "a4-changed-again");
        assert_eq!(load_state(&pair).pending.total(), 0, "nothing waiting after confirming");
        // …but a plan whose actions no longer match is refused.
        w(&l.join("x.txt"), "x");
        let p = plan(&cfg, &pair, Mode::TwoWay).unwrap();
        fs::remove_file(l.join("x.txt")).unwrap();
        assert!(execute(&cfg, &p, &mut |_| {}).is_err(), "stale plan must be refused");

        // 5. Pull mirrors remote -> local, asks before deleting the local-only file.
        w(&l.join("only-local.txt"), "ol");
        let p = plan(&cfg, &pair, Mode::Pull).unwrap();
        assert_eq!(acts(&p), vec![("only-local.txt".into(), Action::DeleteLocal)]);

        // 6. Renames are repeated on the other side instead of delete + copy.
        let p = plan(&cfg, &pair, Mode::TwoWay).unwrap();
        execute(&cfg, &p, &mut |_| {}).unwrap();
        fs::rename(r.join("sub/c.txt"), r.join("sub/c-renamed.txt")).unwrap();
        fs::rename(l.join("e.txt"), l.join("e2.txt")).unwrap();
        let p = plan(&cfg, &pair, Mode::TwoWay).unwrap();
        assert_eq!(
            acts(&p),
            vec![("e2.txt".into(), Action::RenameRemote), ("sub/c-renamed.txt".into(), Action::RenameLocal)]
        );
        assert_eq!(p.items[1].from.as_deref(), Some("sub/c.txt"));
        let rep = execute(&cfg, &p, &mut |_| {}).unwrap();
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert!(has(&l, "sub/c-renamed.txt") && !has(&l, "sub/c.txt"));
        assert!(has(&r, "e2.txt") && !has(&r, "e.txt"));
        assert!(plan(&cfg, &pair, Mode::TwoWay).unwrap().items.is_empty(), "in sync after renames");

        // 7. Safety: emptied remote is never treated as "delete everything".
        let p = plan(&cfg, &pair, Mode::TwoWay).unwrap();
        execute(&cfg, &p, &mut |_| {}).unwrap();
        fs::remove_dir_all(&r).unwrap();
        fs::create_dir_all(&r).unwrap();
        let e = plan(&cfg, &pair, Mode::TwoWay).unwrap_err().to_string();
        assert!(e.contains("Remote folder is empty"), "{e}");

        reset_state(&pair);
        let _ = fs::remove_dir_all(&base);
    }
}
