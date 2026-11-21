DRAISINE: sichere Synchronisation lokaler Ordner mit Google Drive & Co. (über rclone)
===================================================================================

Start
-----
  Linux:    ./draisine           (am besten in den Autostart legen)
  Windows:  draisine.exe
  macOS:    Draisine.app         (beim ersten Start: Rechtsklick > Öffnen, die App ist nicht signiert)

Es erscheint ein Draisinen-Symbol im Tray / in der Menüleiste:
  grau   = Auto-Sync aus          grün   = Auto-Sync an, alles gut
  blau   = Sync läuft             orange = Änderungen warten auf deine Bestätigung
  rot    = Fehler (siehe Log)

Linksklick öffnet das Fenster, Rechtsklick das Menü.
GNOME braucht die Erweiterung "AppIndicator and KStatusNotifierItem Support".

Erste Schritte
--------------
1. Fenster > Einstellungen > rclone: Remote einrichten (z. B. Name "gdrive", Typ "drive").
   Der Browser öffnet sich zur Anmeldung. Ein bereits eingerichtetes rclone wird automatisch genutzt.
2. Ordnerpaar hinzufügen: lokaler Ordner + Remote-Pfad (z. B. gdrive:STEUER), Speichern.
3. "Diff" zeigt die Unterschiede. "2-Wege-Sync" zeigt eine Vorschau; erst "Ausführen" ändert etwas.

Sicherheit
----------
- Vor jedem Löschen oder Überschreiben wirst du gefragt (Vorschau + Bestätigung).
- Lokal gelöschte oder überschriebene Dateien landen im Papierkorb.
- Konflikte (Datei auf beiden Seiten geändert): beide Versionen bleiben erhalten als
    name-YYYY-MM-DDTHH-MM-SSZ-local.ext  und  name-YYYY-MM-DDTHH-MM-SSZ-remote.ext
- Auto-Sync macht nur, was nichts zerstören kann (neue Dateien, Konflikte). Löschen und
  Überschreiben wartet, bis du es bestätigst (Symbol wird orange).
- Ist eine Seite plötzlich leer (nicht eingehängt, falscher Pfad), bricht der Sync ab.

Dateien in diesem Ordner
------------------------
  draisine / draisine.exe   die App
  rclone / rclone.exe       das Transportwerkzeug (https://rclone.org), wird automatisch genutzt
  rclone.conf               (optional) eigene rclone-Konfiguration, wird automatisch genutzt
  draisine-data/            (optional) anlegen für den PORTABLEN Modus: dann liegen Einstellungen,
                            Sync-Status und Logs hier statt im Benutzerprofil.

Logs: Menü > "Log öffnen (TXT)". Logs werden nie automatisch gelöscht.
