//! Thin wrapper around the rclone binary.

use crate::config::{Config, dirs};
use crate::log;
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub size: i64,
    /// Modification time, milliseconds since the epoch.
    pub mtime_ms: i64,
}

pub type Listing = HashMap<String, Entry>;

#[derive(Clone)]
pub struct Rclone {
    pub bin: PathBuf,
    global: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LsJson {
    path: String,
    size: i64,
    mod_time: String,
    is_dir: bool,
}

fn exe_name() -> &'static str {
    if cfg!(windows) { "rclone.exe" } else { "rclone" }
}

/// rclone next to our executable (also `rclone/rclone` and, on macOS, `../Resources/rclone`),
/// else from PATH.
pub fn find_binary(cfg: &Config) -> Option<PathBuf> {
    if !cfg.rclone_path.trim().is_empty() {
        let p = PathBuf::from(cfg.rclone_path.trim());
        return p.exists().then_some(p);
    }
    let d = &dirs().exe_dir;
    for c in [d.join(exe_name()), d.join("rclone").join(exe_name()), d.join("../Resources").join(exe_name())] {
        if c.is_file() {
            return Some(c);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|p| p.join(exe_name())).find(|p| p.is_file())
}

pub fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let _ = cmd;
}

/// Joins a root (local dir or remote path like `gdrive:` / `gdrive:A`) with a relative path.
pub fn join(root: &str, rel: &str) -> String {
    if rel.is_empty() {
        return root.to_string();
    }
    if root.ends_with(':') || root.ends_with('/') || root.ends_with('\\') {
        format!("{root}{rel}")
    } else {
        format!("{root}/{rel}")
    }
}

impl Rclone {
    pub fn new(cfg: &Config) -> Result<Rclone> {
        let bin = find_binary(cfg).ok_or_else(|| {
            anyhow!("rclone nicht gefunden. Lege rclone neben die Draisine-Datei oder installiere es (https://rclone.org/downloads/).")
        })?;
        let mut global = Vec::new();
        let conf = if !cfg.rclone_config.trim().is_empty() {
            Some(PathBuf::from(cfg.rclone_config.trim()))
        } else {
            let p = dirs().exe_dir.join("rclone.conf");
            p.is_file().then_some(p)
        };
        if let Some(c) = conf {
            global.push("--config".into());
            global.push(c.display().to_string());
        }
        global.extend(cfg.extra_flags.iter().filter(|f| !f.trim().is_empty()).cloned());
        Ok(Rclone { bin, global })
    }

    fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args).args(&self.global).stdin(Stdio::null());
        no_window(&mut cmd);
        cmd
    }

    /// Runs rclone, returns (success, stdout, stderr). stderr lines are logged.
    pub fn run(&self, args: &[String]) -> Result<(bool, String, String)> {
        let out = self.command(args).output().with_context(|| format!("{} starten", self.bin.display()))?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        Ok((out.status.success(), stdout, stderr))
    }

    /// Runs rclone and streams stderr lines (progress, transfers) to the log and to `on_line`.
    pub fn run_logged(&self, args: &[String], on_line: &mut dyn FnMut(&str)) -> Result<bool> {
        log::info(format!("rclone {}", args.join(" ")));
        let mut child = self
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("{} starten", self.bin.display()))?;
        let stdout = child.stdout.take().unwrap();
        let out_thread = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
                log::info(format!("rclone: {line}"));
            }
        });
        for line in BufReader::new(child.stderr.take().unwrap()).lines().map_while(|l| l.ok()) {
            log::info(format!("rclone: {line}"));
            on_line(&line);
        }
        let _ = out_thread.join();
        Ok(child.wait()?.success())
    }

    pub fn version(&self) -> String {
        match self.run(&["version".into()]) {
            Ok((_, out, _)) => out.lines().next().unwrap_or("").to_string(),
            Err(e) => e.to_string(),
        }
    }

    /// Configured remotes as (name without colon, type), e.g. ("gdrive", "drive").
    pub fn remotes(&self) -> Vec<(String, String)> {
        match self.run(&["listremotes".into(), "--long".into()]) {
            Ok((true, out, _)) => out
                .lines()
                .filter_map(|l| {
                    let (n, t) = l.split_once(':')?;
                    Some((n.trim().to_string(), t.trim().to_string()))
                })
                .filter(|(n, _)| !n.is_empty())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Sub-folders of a remote path (one level), sorted.
    pub fn list_dirs(&self, path: &str) -> Result<Vec<String>> {
        let (ok, out, err) =
            self.run(&["lsf".into(), "--dirs-only".into(), "--max-depth".into(), "1".into(), path.into()])?;
        if !ok {
            bail!("{}", err.trim());
        }
        let mut v: Vec<String> =
            out.lines().map(|l| l.trim_end_matches('/').to_string()).filter(|l| !l.is_empty()).collect();
        v.sort_by_key(|a| a.to_lowercase());
        Ok(v)
    }

    /// Checks that a remote is reachable (lists its top level).
    pub fn test_remote(&self, name: &str) -> Result<()> {
        let (ok, _, err) =
            self.run(&["lsf".into(), "--dirs-only".into(), "--max-depth".into(), "1".into(), format!("{name}:")])?;
        if !ok {
            bail!("{}", err.trim().lines().last().unwrap_or("Verbindung fehlgeschlagen"));
        }
        Ok(())
    }

    /// Starts `rclone config create <name> <type>` (opens the browser for OAuth providers).
    pub fn spawn_config_create(&self, name: &str, typ: &str) -> Result<std::process::Child> {
        let mut args = vec!["config".to_string(), "create".into(), name.into(), typ.into()];
        if typ == "drive" {
            args.push("scope=drive".into());
        }
        log::info(format!("rclone config create {name} {typ}"));
        Ok(self.command(&args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?)
    }

    /// Recursive file listing. Returns Ok(None) if the directory does not exist.
    pub fn list(&self, root: &str, excludes: &[String]) -> Result<Option<Listing>> {
        let mut args: Vec<String> =
            ["lsjson", "-R", "--files-only", "--no-mimetype", root].iter().map(|s| s.to_string()).collect();
        for e in excludes.iter().filter(|e| !e.trim().is_empty()) {
            args.push("--exclude".into());
            args.push(e.trim().into());
        }
        let (ok, out, err) = self.run(&args)?;
        if !ok {
            if err.to_lowercase().contains("directory not found") {
                return Ok(None);
            }
            bail!("Auflisten von {root} fehlgeschlagen:\n{}", err.trim());
        }
        let items: Vec<LsJson> = serde_json::from_str(&out).context("lsjson-Ausgabe lesen")?;
        let mut map = Listing::new();
        for it in items.into_iter().filter(|i| !i.is_dir) {
            let mtime_ms = chrono::DateTime::parse_from_rfc3339(&it.mod_time)
                .map(|t| t.timestamp_millis())
                .unwrap_or(0);
            map.insert(it.path, Entry { size: it.size, mtime_ms });
        }
        Ok(Some(map))
    }

    /// Compares the given files by content (hash; by download if configured and no common hash).
    /// Returns the set of paths that are identical on both sides.
    pub fn identical(&self, local: &str, remote: &str, paths: &[String], download: bool) -> Result<Vec<String>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let list = tmp_file("check-list", &paths.join("\n"))?;
        let combined = tmp_file("check-out", "")?;
        let mut args: Vec<String> = vec![
            "check".into(),
            local.into(),
            remote.into(),
            "--files-from-raw".into(),
            list.display().to_string(),
            "--combined".into(),
            combined.display().to_string(),
        ];
        if download {
            args.push("--download".into());
        }
        let (_ok, _out, err) = self.run(&args)?; // exit code != 0 when differences exist
        let text = std::fs::read_to_string(&combined).unwrap_or_default();
        let _ = std::fs::remove_file(&list);
        let _ = std::fs::remove_file(&combined);
        if text.trim().is_empty() && !err.trim().is_empty() {
            bail!("rclone check fehlgeschlagen:\n{}", err.trim());
        }
        Ok(text.lines().filter_map(|l| l.strip_prefix("= ")).map(str::to_string).collect())
    }
}

pub fn tmp_file(prefix: &str, content: &str) -> Result<PathBuf> {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let p = dirs().runtime_dir.join(format!("{prefix}-{}-{n}.txt", std::process::id()));
    std::fs::write(&p, content)?;
    Ok(p)
}
