# Draisine

Tray-App für sichere Synchronisation lokaler Ordner mit Google Drive, OneDrive, Dropbox und Co.
[rclone](https://rclone.org) übernimmt dabei nur den Transport. Benannt nach Karl Drais' Laufmaschine (Mannheim, 1817), die zwischen zwei Punkten hin- und herfährt.

- **Diff** lokal ↔ remote
- **1-Weg-Sync** remote ➡ lokal und lokal ➡ remote (Spiegeln)
- **2-Wege-Sync** lokal ↔ remote
- **Auto-Sync** an/aus (2-Wege, im Hintergrund)

Eine ausführbare Datei, keine Installation. Im Release-Archiv liegt das passende rclone direkt daneben.
Linux (KDE, GNOME mit AppIndicator-Erweiterung, …), Windows, macOS.

## Sicherheitsmodell

| Situation | Verhalten |
|---|---|
| Löschen oder Überschreiben | Wird **immer** vorher angezeigt und muss bestätigt werden (Vorschau mit Haken, dann Bestätigungsdialog). |
| Lokal löschen / überschreiben | Die alte Datei kommt in den **Papierkorb** des Systems. Schlägt das fehl, wird nicht gelöscht bzw. überschrieben. |
| Remote löschen / überschreiben | Optional in einen Backup-Ordner pro Paar (`gdrive:_draisine-backup/<Zeitstempel>/…`), sonst Papierkorb bzw. Versionen des Anbieters. |
| Konflikt (beide Seiten geändert) | Beide Versionen bleiben auf beiden Seiten erhalten: `name-YYYY-MM-DDTHH-MM-SSZ-local.ext` und `name-…-remote.ext`. |
| Auto-Sync | Führt nur Aktionen aus, die nichts zerstören (neue Dateien, Konflikte). Löschen und Überschreiben wartet: Das Tray-Symbol wird orange, und es erscheint eine Benachrichtigung. |
| Vorschau veraltet | Vor dem Ausführen wird neu verglichen. Hat sich eine ausgewählte Aktion geändert, wird nichts ausgeführt. |
| Eine Seite leer oder nicht da | Abbruch, wenn beim letzten Sync dort Dateien lagen (z. B. Laufwerk nicht eingehängt). |
| Massenlöschung | Ab einem einstellbaren Anteil (Standard 25 %) sind Löschungen nicht vorausgewählt, dazu kommt eine Warnung. |
| Gleichzeitige Syncs | Eine Sperrdatei verhindert, dass Tray (Auto-Sync) und Fenster gleichzeitig Dateien ändern. |
| Inhaltsvergleich | Gleiche Größe und Zeit reichen nicht. Ohne verifizierten Sync-Stand wird per Hash verglichen (`rclone check`). |

Alles wird geloggt: eine Datei pro Tag (`draisine-YYYY-MM-DD.txt`). Logs werden nie gelöscht und lassen sich über das Menü „Log öffnen (TXT)“ öffnen.

## Wie der 2-Wege-Sync entscheidet

Pro Ordnerpaar speichert Draisine den Stand beider Seiten nach dem letzten erfolgreichen Sync (Größe und Änderungszeit, `state/<id>.json`). Beim nächsten Lauf wird jede Seite mit **ihrem eigenen** letzten Stand verglichen:

| lokal \ remote | unverändert | neu / geändert | gelöscht |
|---|---|---|---|
| **unverändert** | nichts | ⬅ lokal überschreiben (fragt) | lokal löschen (fragt, Papierkorb) |
| **neu / geändert** | ➡ remote überschreiben (fragt) | gleicher Inhalt: nichts, sonst **Konflikt** | ➡ wiederherstellen |
| **gelöscht** | remote löschen (fragt) | ⬅ wiederherstellen | nichts |

Neue Dateien, die es nur auf einer Seite gibt, werden kopiert. Beim ersten Sync gibt es noch keinen Stand. Dann wird nie gelöscht, und unterschiedliche Dateien werden zu Konflikten. Nicht bestätigte Aktionen behalten ihren alten Stand und erscheinen beim nächsten Mal wieder.

1-Weg-Sync spiegelt die Quelle: Dateien, die nur am Ziel liegen, werden gelöscht, abweichende überschrieben. Beides geschieht nur nach Bestätigung.

## Bauen

```sh
cargo build --release              # target/release/draisine
cargo test                          # Unit-Tests
cargo test -- --ignored --test-threads=1   # End-to-end mit echtem rclone (lokaler Ordner als "Remote")
scripts/package.sh                  # dist/draisine-<ver>-<os>-<arch>.tar.gz|zip, inkl. rclone
```

Linux: Das Binary hängt nur von der libc ab, Grafik und Wayland/X11 werden zur Laufzeit geladen. Der Tray nutzt StatusNotifierItem über D-Bus (kein GTK nötig).
GitHub Actions (`.github/workflows/build.yml`) baut für Linux x86_64, Windows x86_64 und macOS arm64/x86_64. Ein Tag `v*` erzeugt ein Release.

## Aufbau

```
src/main.rs     Einstieg: ohne Argumente Tray, mit --window [diff|pull|push|twoway] das Fenster
src/tray.rs     Tray-Icon + Menü (tray-icon/winit), Auto-Sync-Worker, Benachrichtigungen
src/gui.rs      Fenster (egui): Sync, Vorschau/Bestätigung, Einstellungen, Logs
src/engine.rs   Planung, Konflikte, Ausführung, Sync-Status, Sperre
src/rclone.rs   rclone-Aufrufe (lsjson, check, copy, moveto, deletefile)
src/config.rs   Konfiguration (TOML), Speicherorte, portabler Modus
src/ipc.rs      Tray ↔ Fenster (Sperrdateien + Anfragedatei)
src/icon.rs     Draisinen-Icon, zur Laufzeit gezeichnet (Farbe = Status)
```

Das Fenster läuft als eigener Prozess. Der Tray startet es bei Bedarf, und ein offenes Fenster übernimmt neue Anfragen. Das funktioniert auf allen drei Systemen gleich.

## Speicherorte

| | Linux | Windows | macOS |
|---|---|---|---|
| Konfiguration | `~/.config/draisine/config.toml` | `%APPDATA%\hekori\draisine\config\config.toml` | `~/Library/Application Support/de.hekori.draisine/config.toml` |
| Status, Logs | `~/.local/share/draisine/{state,logs}` | `%APPDATA%\hekori\draisine\data\…` | `~/Library/Application Support/de.hekori.draisine/…` |

**Portabler Modus:** Gibt es neben dem Programm einen Ordner `draisine-data`, liegt alles dort. Ein `rclone.conf` neben dem Programm wird automatisch verwendet.
