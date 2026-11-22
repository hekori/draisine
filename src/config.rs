//! Configuration, directories and the portable mode.
//!
//! Portable mode: if a folder `draisine-data` exists next to the executable, config, state and
//! logs are stored there, so the whole app (binary + rclone + data) lives in one folder.
//! Otherwise the OS standard directories are used.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const APP_NAME: &str = "Draisine";
pub const PORTABLE_DIR: &str = "draisine-data";
/// Default auto-sync interval of a new folder pair, in minutes.
pub const DEFAULT_INTERVAL: u32 = 15;

#[derive(Debug, Clone)]
pub struct Dirs {
    pub config_file: PathBuf,
    pub state_dir: PathBuf,
    pub log_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub exe_dir: PathBuf,
    pub portable: bool,
}

static DIRS: OnceLock<Dirs> = OnceLock::new();

pub fn dirs() -> &'static Dirs {
    DIRS.get_or_init(|| {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok())
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        let portable_root = exe_dir.join(PORTABLE_DIR);
        let d = if portable_root.is_dir() {
            Dirs {
                config_file: portable_root.join("config.toml"),
                state_dir: portable_root.join("state"),
                log_dir: portable_root.join("logs"),
                runtime_dir: portable_root.join("run"),
                exe_dir,
                portable: true,
            }
        } else {
            let pd = directories::ProjectDirs::from("de", "hekori", "draisine")
                .expect("no home directory");
            Dirs {
                config_file: pd.config_dir().join("config.toml"),
                state_dir: pd.data_dir().join("state"),
                log_dir: pd.data_dir().join("logs"),
                runtime_dir: pd.data_dir().join("run"),
                exe_dir,
                portable: false,
            }
        };
        for p in [d.config_file.parent().unwrap(), &d.state_dir, &d.log_dir, &d.runtime_dir] {
            let _ = std::fs::create_dir_all(p);
        }
        d
    })
}

/// One local folder <-> one remote folder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Pair {
    /// Stable id, used for the state file name.
    pub id: String,
    pub name: String,
    /// Local folder, e.g. /home/me/GOOGLE_DRIVE/STEUER
    pub local: String,
    /// rclone remote path, e.g. gdrive:STEUER
    pub remote: String,
    /// rclone exclude patterns (relative to the pair root), e.g. "2020/GEHALT/**"
    pub excludes: Vec<String>,
    /// Optional rclone path where remote files are moved before they are overwritten or
    /// deleted, e.g. gdrive:_draisine-backup. Empty: rely on the provider's trash / versions.
    pub remote_backup: String,
    /// Included in the background auto-sync. Only takes effect once the pair has been synced
    /// manually at least once (see `engine::has_synced`).
    pub auto_sync: bool,
    /// Auto-sync runs this pair every N minutes.
    pub interval_minutes: u32,
}

impl Default for Pair {
    fn default() -> Self {
        Pair {
            id: new_id(),
            name: String::new(),
            local: String::new(),
            remote: String::new(),
            excludes: Vec::new(),
            remote_backup: String::new(),
            auto_sync: false,
            interval_minutes: DEFAULT_INTERVAL,
        }
    }
}

impl Pair {
    pub fn label(&self) -> String {
        if self.name.trim().is_empty() {
            format!("{} ↔ {}", self.local, self.remote)
        } else {
            self.name.clone()
        }
    }

    /// Changes in these fields invalidate the stored sync state.
    pub fn signature(&self) -> String {
        format!("{}|{}|{}", self.local, self.remote, self.excludes.join("\u{1f}"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub pairs: Vec<Pair>,
    /// Set from the tray menu: no pair auto-syncs while this is on.
    pub auto_sync_paused: bool,
    /// Former global switch and interval, only read to migrate old configs (now per pair).
    #[serde(skip_serializing)]
    pub auto_sync: bool,
    #[serde(skip_serializing)]
    pub interval_minutes: u32,
    /// Path to rclone. Empty: next to the executable, then PATH.
    pub rclone_path: String,
    /// Path to rclone.conf. Empty: rclone.conf next to the executable, then rclone's default.
    pub rclone_config: String,
    /// Extra flags passed to every rclone call.
    pub extra_flags: Vec<String>,
    /// Plans deleting more than this share of the files are not preselected and show a warning.
    pub max_delete_percent: u32,
    /// Compare file contents by downloading when no common hash exists (slow, exact).
    pub check_download: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            pairs: Vec::new(),
            auto_sync_paused: false,
            auto_sync: false,
            interval_minutes: DEFAULT_INTERVAL,
            rclone_path: String::new(),
            rclone_config: String::new(),
            extra_flags: vec!["--drive-skip-gdocs".into()],
            max_delete_percent: 25,
            check_download: false,
        }
    }
}

impl Config {
    pub fn load() -> Config {
        let path = &dirs().config_file;
        match std::fs::read_to_string(path) {
            Ok(s) => match toml::from_str::<Config>(&s) {
                Ok(mut c) => {
                    migrate_pair_auto_sync(&s, &mut c);
                    c
                }
                Err(e) => {
                    crate::log::error(format!("Configuration {} is invalid: {e}", path.display()));
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = &dirs().config_file;
        let s = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, s).with_context(|| format!("{}", tmp.display()))?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn mtime() -> Option<std::time::SystemTime> {
        std::fs::metadata(&dirs().config_file).and_then(|m| m.modified()).ok()
    }
}

/// Configs from before per-pair auto-sync: pairs without the keys take over the global settings.
fn migrate_pair_auto_sync(raw: &str, c: &mut Config) {
    let Ok(t) = raw.parse::<toml::Table>() else { return };
    let Some(pairs) = t.get("pairs").and_then(|p| p.as_array()) else { return };
    for (pair, raw) in c.pairs.iter_mut().zip(pairs) {
        let Some(r) = raw.as_table() else { continue };
        if !r.contains_key("auto_sync") {
            pair.auto_sync = c.auto_sync;
        }
        if !r.contains_key("interval_minutes") {
            pair.interval_minutes = c.interval_minutes;
        }
    }
}

pub fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("p{:x}", n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_configs_take_over_the_global_auto_sync() {
        let raw = "auto_sync = true\ninterval_minutes = 7\n[[pairs]]\nid = \"a\"\n[[pairs]]\nid = \"b\"\nauto_sync = false\ninterval_minutes = 30\n";
        let mut c: Config = toml::from_str(raw).unwrap();
        migrate_pair_auto_sync(raw, &mut c);
        assert!(c.pairs[0].auto_sync, "missing key: inherits the global switch");
        assert!(!c.pairs[1].auto_sync, "explicit value is kept");
        assert_eq!((c.pairs[0].interval_minutes, c.pairs[1].interval_minutes), (7, 30));
        let saved = toml::to_string(&c).unwrap();
        let top = saved.parse::<toml::Table>().unwrap();
        assert!(!top.contains_key("auto_sync") && !top.contains_key("interval_minutes"), "old globals are not written back");
    }
}
