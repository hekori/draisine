//! Thin wrapper around the rclone binary.

use crate::config::{Config, dirs};
use crate::log;
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set to true to stop the running rclone call (the process is killed) and all later ones.
pub type Cancel = Arc<AtomicBool>;

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
    cancel: Option<Cancel>,
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
            anyhow!("rclone not found. Put rclone next to the Draisine program or install it (https://rclone.org/downloads/).")
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
        Ok(Rclone { bin, global, cancel: None })
    }

    /// Makes [`Rclone::run`] calls abortable: once `c` is set, the running rclone process is
    /// killed and every call fails with "Cancelled".
    pub fn with_cancel(mut self, c: Cancel) -> Rclone {
        self.cancel = Some(c);
        self
    }

    fn cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))
    }

    fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args).args(&self.global).stdin(Stdio::null());
        no_window(&mut cmd);
        cmd
    }

    /// Runs rclone, returns (success, stdout, stderr). stderr lines are logged.
    pub fn run(&self, args: &[String]) -> Result<(bool, String, String)> {
        if self.cancel.is_none() {
            let out = self.command(args).output().with_context(|| format!("{} starten", self.bin.display()))?;
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            return Ok((out.status.success(), stdout, stderr));
        }
        if self.cancelled() {
            bail!("Cancelled");
        }
        let mut child = self
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("{} starten", self.bin.display()))?;
        // Drain both pipes on threads so rclone never blocks on a full pipe while we poll.
        let drain = |r: Option<Box<dyn std::io::Read + Send>>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut r) = r {
                    let _ = r.read_to_end(&mut buf);
                }
                String::from_utf8_lossy(&buf).to_string()
            })
        };
        let out_t = drain(child.stdout.take().map(|r| Box::new(r) as _));
        let err_t = drain(child.stderr.take().map(|r| Box::new(r) as _));
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if self.cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                let _ = (out_t.join(), err_t.join());
                log::info("rclone cancelled");
                bail!("Cancelled");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        let stdout = out_t.join().unwrap_or_default();
        let stderr = err_t.join().unwrap_or_default();
        Ok((status.success(), stdout, stderr))
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
            bail!("{}", err.trim().lines().last().unwrap_or("Connection failed"));
        }
        Ok(())
    }

    /// Starts `rclone config create <name> <type> key=value …` for backends configured by
    /// plain parameters (S3, WebDAV, SFTP). `--obscure` makes rclone always obscure password
    /// fields, even if the value happens to look already obscured.
    pub fn spawn_config_create_params(&self, name: &str, typ: &str, params: &[(String, String)]) -> Result<std::process::Child> {
        let mut args = vec!["config".to_string(), "create".into(), name.into(), typ.into()];
        args.extend(params.iter().map(|(k, v)| format!("{k}={v}")));
        args.push("--obscure".into());
        let keys: Vec<&str> = params.iter().map(|(k, _)| k.as_str()).collect();
        log::info(format!("rclone config create {name} {typ} ({})", keys.join(", ")));
        Ok(self.command(&args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?)
    }

    /// Starts `rclone config create <name> <type>` (opens the browser for OAuth providers).
    pub fn spawn_config_create(&self, name: &str, typ: &str) -> Result<std::process::Child> {
        let mut args = vec!["config".to_string(), "create".into(), name.into(), typ.into()];
        if typ == "drive" {
            args.push("scope=drive".into());
        }
        // Own Dropbox app (consent screen shows "Draisine" instead of "rclone").
        // Baked in at build time; without them rclone's built-in client is used.
        if typ == "dropbox" {
            let key = option_env!("DRAISINE_DROPBOX_APP_KEY").filter(|s| !s.is_empty());
            let secret = option_env!("DRAISINE_DROPBOX_APP_SECRET").filter(|s| !s.is_empty());
            if let (Some(id), Some(secret)) = (key, secret) {
                args.push(format!("client_id={id}"));
                args.push(format!("client_secret={secret}"));
            }
        }
        // Branded page shown in the browser after the OAuth redirect (instead of rclone's
        // "All done. Please go back to rclone."). rclone reads it only during auth and
        // does not store `config_template_file` in rclone.conf.
        let page = dirs().runtime_dir.join("auth_page.html");
        match std::fs::write(&page, AUTH_PAGE) {
            Ok(()) => args.push(format!("config_template_file={}", page.display())),
            Err(e) => log::info(format!("Sign-in page not written ({e}), using rclone default")),
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
            bail!("Listing {root} failed:\n{}", err.trim());
        }
        let items: Vec<LsJson> = serde_json::from_str(&out).context("reading lsjson output")?;
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
        let res = self.run(&args); // exit code != 0 when differences exist
        let text = std::fs::read_to_string(&combined).unwrap_or_default();
        let _ = std::fs::remove_file(&list);
        let _ = std::fs::remove_file(&combined);
        let (_ok, _out, err) = res?;
        if text.trim().is_empty() && !err.trim().is_empty() {
            bail!("rclone check failed:\n{}", err.trim());
        }
        Ok(text.lines().filter_map(|l| l.strip_prefix("= ")).map(str::to_string).collect())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LsHash {
    path: String,
    #[serde(default)]
    hashes: HashMap<String, String>,
}

impl Rclone {
    /// Hashes of the given files (relative to `root`): path -> (hash type -> value).
    /// Without `hash_type` the backend reports the types it supports natively.
    pub fn hashes(
        &self,
        root: &str,
        paths: &[String],
        hash_type: Option<&str>,
    ) -> Result<HashMap<String, HashMap<String, String>>> {
        if paths.is_empty() {
            return Ok(HashMap::new());
        }
        let list = tmp_file("hash-list", &paths.join("\n"))?;
        let mut args: Vec<String> = ["lsjson", "-R", "--files-only", "--no-mimetype", "--hash", "--files-from-raw"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        args.push(list.display().to_string());
        args.push(root.into());
        if let Some(t) = hash_type {
            args.push("--hash-type".into());
            args.push(t.into());
        }
        let res = self.run(&args);
        let _ = std::fs::remove_file(&list);
        let (ok, out, err) = res?;
        if !ok {
            bail!("Hashing in {root} failed:\n{}", err.trim());
        }
        let items: Vec<LsHash> = serde_json::from_str(&out).context("reading lsjson output")?;
        Ok(items.into_iter().map(|i| (i.path, i.hashes)).collect())
    }

    /// Up to `max` bytes of a file (`None` if it is larger than that).
    pub fn cat(&self, path: &str, max: u64) -> Result<Option<String>> {
        let (ok, out, err) = self.run(&["cat".into(), "--count".into(), (max + 1).to_string(), path.into()])?;
        if !ok {
            bail!("Reading {path} failed:\n{}", err.trim());
        }
        Ok((out.len() as u64 <= max).then_some(out))
    }
}

/// Go template rclone renders after the OAuth redirect (fields: .OK, .Name, .Description, .Code, .HelpURL).
const AUTH_PAGE: &str = include_str!("../assets/auth_page.html");

pub fn tmp_file(prefix: &str, content: &str) -> Result<PathBuf> {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let p = dirs().runtime_dir.join(format!("{prefix}-{}-{n}.txt", std::process::id()));
    std::fs::write(&p, content)?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs rclone. Run with: cargo test -- --ignored --test-threads=1
    #[test]
    #[ignore]
    fn cancel_kills_running_rclone() {
        let cancel: Cancel = Default::default();
        let rc = Rclone::new(&Config::default()).unwrap().with_cancel(cancel.clone());
        let c = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            c.store(true, Ordering::Relaxed);
        });
        let t = std::time::Instant::now();
        // Recursive listing of / runs far longer than the test allows.
        let r = rc.run(&["lsjson".into(), "-R".into(), "/".into()]);
        assert!(r.is_err_and(|e| e.to_string() == "Cancelled"));
        assert!(t.elapsed() < std::time::Duration::from_secs(2), "took {:?}", t.elapsed());
        // Later calls fail immediately without starting rclone.
        assert!(rc.run(&["version".into()]).is_err());
    }
}
