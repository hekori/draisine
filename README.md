# Draisine

Tray app for safely syncing local folders with Google Drive, OneDrive, Dropbox and others.
[rclone](https://rclone.org) only handles the transfer. Named after Karl Drais' running machine (Mannheim, 1817), which shuttles back and forth between two points.

- **Diff** local ↔ remote
- **One-way sync** remote ➡ local and local ➡ remote (mirroring)
- **Two-way sync** local ↔ remote
- **Auto-sync** on/off (two-way, in the background)

A single executable, no installation. The release archive ships the matching rclone right next to it.
Linux (KDE, GNOME with the AppIndicator extension, …), Windows, macOS.

## Safety model

| Situation | Behaviour |
|---|---|
| Delete or overwrite | **Always** shown first and must be confirmed (preview with checkboxes, then a confirmation dialog). |
| Delete / overwrite locally | The old file goes to the system **trash**. If that fails, nothing is deleted or overwritten. |
| Delete / overwrite remotely | Optionally into a per-pair backup folder (`gdrive:_draisine-backup/<timestamp>/…`), otherwise the provider's trash or version history. |
| Conflict (changed on both sides) | Both versions are kept on both sides: `name-YYYY-MM-DDTHH-MM-SSZ-local.ext` and `name-…-remote.ext`. |
| Auto-sync | Only performs non-destructive actions (new files, conflicts). Deletes and overwrites wait: the tray icon turns orange and a notification appears. |
| Stale preview | Everything is compared again before applying. If a selected action has changed, nothing is applied. |
| One side empty or missing | Stops if there were files there at the last sync (e.g. drive not mounted). |
| Mass deletion | Above a configurable share (default 25 %) deletions are not preselected, and a warning is shown. |
| Concurrent syncs | A lock file prevents the tray (auto-sync) and the window from changing files at the same time. |
| Content comparison | Same size and time are not enough. Without a verified sync state, files are compared by hash (`rclone check`). |

Everything is logged: one file per day (`draisine-YYYY-MM-DD.txt`). Logs are never deleted and can be opened via the menu entry "Open log (TXT)".

## How two-way sync decides

For each folder pair, Draisine stores the state of both sides after the last successful sync (size and modification time, `state/<id>.json`). On the next run, each side is compared with **its own** last state:

| local \ remote | unchanged | new / changed | deleted |
|---|---|---|---|
| **unchanged** | nothing | ⬅ overwrite local (asks) | delete local (asks, trash) |
| **new / changed** | ➡ overwrite remote (asks) | same content: nothing, otherwise **conflict** | ➡ restore |
| **deleted** | delete remote (asks) | ⬅ restore | nothing |

New files that exist on only one side are copied. On the first sync there is no state yet. Then nothing is ever deleted, and differing files become conflicts. Unconfirmed actions keep their old state and show up again next time.

One-way sync mirrors the source: files that exist only at the destination are deleted, differing ones overwritten. Both happen only after confirmation.

On first start (no folder pair yet) a setup assistant opens automatically. It checks rclone, creates a new connection if needed (sign-in in the browser via `rclone config create`) and guides you to choosing the folders.

## Building

```sh
cargo build --release              # target/release/draisine
cargo test                          # unit tests
cargo test -- --ignored --test-threads=1   # end-to-end with real rclone (local folder as "remote")
# Debug builds only, for screenshots: DRAISINE_DEBUG_STEP=0..5 (assistant), DRAISINE_DEBUG_PAGE=conn|log|settings,
# DRAISINE_DEBUG_THEME=light, DRAISINE_DEBUG_CONFIRM=1
scripts/package.sh                  # dist/draisine-<ver>-<os>-<arch>.tar.gz|zip, including rclone
```

Own Dropbox app: set `DRAISINE_DROPBOX_APP_KEY` and `DRAISINE_DROPBOX_APP_SECRET` at build time (locally in the gitignored `.cargo/config.toml` under `[env]`, in CI as repository secrets). Without them, rclone's built-in client is used.

Linux: the binary only depends on libc; graphics and Wayland/X11 are loaded at runtime. The tray uses StatusNotifierItem over D-Bus (no GTK needed).
GitHub Actions (`.github/workflows/build.yml`) builds for Linux x86_64, Windows x86_64 and macOS arm64/x86_64. A `v*` tag creates a release.

## Structure

```
src/main.rs     Entry point: no arguments = tray, --window [diff|pull|push|twoway] = window
src/tray.rs     Tray icon + menu (tray-icon/winit), auto-sync worker, notifications
src/gui/        Window (egui): overview with folder pairs, preview/confirmation, connections, log, settings
  theme.rs      Colours (light/dark following the system), font (Noto Sans, embedded), cards, buttons, toggles
  wizard.rs     Setup assistant on first start (rclone → provider → browser sign-in → folders)
  browser.rs    Folder browser for the cloud
src/engine.rs   Planning, conflicts, execution, sync state, lock
src/rclone.rs   rclone calls (lsjson, check, copy, moveto, deletefile)
src/config.rs   Configuration (TOML), storage locations, portable mode
src/ipc.rs      Tray ↔ window (lock files + request file)
src/icon.rs     Draisine icon, drawn at runtime (colour = status)
assets/         Fonts, the logo (source + 256px mask, `scripts/make_logo_mask.py`), the page shown after sign-in
```

The window runs as a separate process. The tray starts it when needed, and an open window takes over new requests. This works the same on all three systems.

## Storage locations

| | Linux | Windows | macOS |
|---|---|---|---|
| Configuration | `~/.config/draisine/config.toml` | `%APPDATA%\hekori\draisine\config\config.toml` | `~/Library/Application Support/de.hekori.draisine/config.toml` |
| State, logs | `~/.local/share/draisine/{state,logs}` | `%APPDATA%\hekori\draisine\data\…` | `~/Library/Application Support/de.hekori.draisine/…` |

**Portable mode:** if there is a `draisine-data` folder next to the program, everything is stored there. An `rclone.conf` next to the program is used automatically.

## License

MIT, see [LICENSE](LICENSE). Third-party components (bundled rclone, fonts, Rust crates) and their licenses are listed in [assets/THIRD_PARTY.txt](assets/THIRD_PARTY.txt) and on the About page.
